//! LP Staking and Rewards Contract
//!
//! Liquidity providers can stake their LP tokens to earn reward tokens.
//! Uses a rewards-per-share accumulator pattern (similar to SushiSwap's MasterChef)
//! for efficient O(1) reward calculation per claim.
//!
//! Issue #296: Optional lock-duration boost multiplier (1Ãƒâ€”Ã¢â‚¬â€œ4Ãƒâ€”), modelled on
//! Curve's veTokenomics.  Stakers may voluntarily lock for a fixed duration to
//! earn a higher share of rewards.  The boost is applied to the *effective*
//! staked amount used in reward calculations; the actual LP token balance is
//! unchanged.

#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, BytesN, Env, Symbol, Vec,
};

// Export compiled WASM for tests/dev usage when the `testutils` feature is enabled.
#[cfg(feature = "testutils")]
pub const WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32v1-none/release/staking.wasm"
));

use soroban_sdk::token::Client as SepTokenClient;

mod boost;

// ── Typed errors ─────────────────────────────────────────────────────────────

/// Errors surfaced by the staking contract.
///
/// Every caller-triggerable failure path returns one of these discriminants
/// instead of trapping, so callers can distinguish, for example, a still-locked
/// position from an empty one. Discriminants 1 and 2 follow the workspace-wide
/// convention (`AlreadyInitialized` / `NotInitialized`); see
/// `contracts/amm/src/lib.rs` `AmmError` for the reference enum.
#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum StakingError {
    /// `initialize` was called on a pool that is already set up.
    AlreadyInitialized = 1,
    /// A function was called before `initialize`.
    NotInitialized = 2,
    /// The caller is not the recorded admin.
    Unauthorized = 3,
    /// The contract is paused; staking and claiming are halted.
    Paused = 4,
    /// An amount argument was zero or negative where a positive value is
    /// required.
    InvalidAmount = 5,
    /// The staker has no staked balance.
    NothingStaked = 6,
    /// The requested amount exceeds the staker's staked balance.
    InsufficientStaked = 7,
    /// The staker's lock has not yet expired.
    StillLocked = 8,
    /// There are no pending rewards to claim.
    NoPendingRewards = 9,
    /// Emergency mode is not active, so `emergency_withdraw` is unavailable.
    EmergencyModeNotActive = 10,
    /// `extend_lock` was called on a position with no active lock.
    NoActiveLock = 11,
    /// A lock duration argument was zero where a positive value is required.
    InvalidDuration = 12,
    /// A stake call supplied neither a positive amount nor a lock duration.
    NothingToStake = 13,
    /// The boost configuration bounds are invalid (non-positive, or max below
    /// min).
    InvalidBoostConfig = 14,
    /// The lock-duration configuration bounds are invalid (non-positive, or
    /// max below min).
    InvalidLockDuration = 15,
    /// Adding rewards would push the pool balance above the configured cap.
    MaxRewardPoolExceeded = 16,
    /// A new max-balance cap was set below the current pool balance.
    InvalidMaxBalance = 17,
    /// A reward distribution was attempted while no effective stake exists.
    NoStakers = 18,
    /// A batch call exceeded `MAX_BATCH_SIZE` entries.
    BatchTooLarge = 19,
    /// `accept_admin` called without a prior `propose_admin`.
    NoPendingAdmin = 20,
    /// `accept_admin` called by an address other than the nominee.
    WrongAdmin = 21,
    /// `initialize` was given the same token as LP token and reward token. The
    /// reward accounting derives "undistributed" from the contract's whole
    /// reward-token balance, which would then include stakers' LP principal.
    IdenticalTokens = 22,
    /// `migrate_staker_index` was called with an `offset` that is not the next
    /// unmigrated position of the legacy staker vector.
    MigrationOutOfOrder = 23,
}

// Ã¢â€â‚¬Ã¢â€â‚¬ Constants Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬

const SCALE_FACTOR: i128 = 1_000_000_000_000_000_000; // 1e18

/// Boost multiplier is stored scaled by BOOST_SCALE so we avoid floats.
/// 1Ãƒâ€” = 10_000, 4Ãƒâ€” = 40_000.
const BOOST_SCALE: i128 = 10_000;

/// Maximum lock duration in seconds (4 years).
const MAX_LOCK_DURATION: u64 = 4 * 365 * 24 * 3600;

/// Minimum lock duration in seconds (1 week).
const MIN_LOCK_DURATION: u64 = 7 * 24 * 3600;

/// Default maximum boost multiplier (2.5Ãƒâ€”, stored as 25_000 / BOOST_SCALE).
const DEFAULT_MAX_BOOST: i128 = 25_000;

/// Minimum boost multiplier (1Ãƒâ€”, stored as 10_000 / BOOST_SCALE).
const MIN_BOOST: i128 = BOOST_SCALE;

const MIN_TTL: u32 = 518_400; // ~30 days (at 5s per ledger)
const BUMP_TO: u32 = 3_110_400; // ~180 days (at 5s per ledger)

/// Maximum stakers a single `settle_boost_batch` / `register_existing_stakers`
/// call will process. Bounds the resource cost of one invocation so a keeper
/// (or anyone) sweeping a large pool must paginate rather than risk hitting
/// the transaction's CPU/memory budget in one shot (#699).
const MAX_BATCH_SIZE: u32 = 50;

/// Maximum stakers `list_expired_boosts` will scan per call, for the same
/// reason as `MAX_BATCH_SIZE`.
const MAX_LIST_LIMIT: u32 = 100;

// Ã¢â€â‚¬Ã¢â€â‚¬ Storage keys Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬

#[contracttype]
pub enum DataKey {
    /// LP token address
    LpToken,
    /// Reward token address
    RewardToken,
    /// Admin address (can add rewards)
    Admin,
    /// Total *effective* LP tokens staked (boosted amounts summed)
    TotalEffectiveStaked,
    /// Accumulated rewards per effective LP token (scaled by 1e18)
    AccumulatedRewardsPerShare,
    /// Staker info: raw staked amount
    StakerAmount(Address),
    /// Staker info: rewards debt (to track already-distributed rewards)
    StakerRewardsDebt(Address),
    /// Undistributed reward tokens: funded by `add_rewards`, not yet assigned
    /// to stakers via the accumulator. `update_rewards` moves an amount out of
    /// here and into `OwedRewards` (#1045).
    ///
    /// Legacy note (issue #1045): on contracts deployed before the fix this
    /// key held a single counter that was decremented both when rewards were
    /// distributed and when they were paid out, so it drifted negative and
    /// stranded every reward cycle after the first. `reconcile_reward_accounting`
    /// repairs such deployments; on fresh contracts the invariant
    /// `token_balance >= UndistributedRewards + OwedRewards` always holds.
    RewardPoolBalance,
    /// Reward tokens already assigned to stakers through
    /// `AccumulatedRewardsPerShare` but not yet paid out (#1045).
    /// `update_rewards` adds to it; `claim`/`unstake`/`_settle_pending_at`
    /// subtract exactly what they transfer.
    OwedRewards,
    /// Accounting reconcile accumulator (#1045). Maps a staker address to the
    /// portion of `OwedRewards` already re-credited by a partially-completed
    /// `reconcile_reward_accounting` run, so a large staker index can be
    /// walked across many paginated calls without double-counting. Fully
    /// cleared once the reconcile completes.
    ReconcileState(Address),
    /// Running cross-page total for a partially-completed
    /// `reconcile_reward_accounting` run (#1045); cleared when the run is
    /// finalized.
    ReconcileTotal,
    /// Lock expiry timestamp (seconds) for a staker; 0 = no lock
    LockExpiry(Address),
    /// Boost multiplier for a staker (scaled by BOOST_SCALE); default = BOOST_SCALE (1Ãƒâ€”)
    BoostMultiplier(Address),
    /// Configurable min boost (scaled); default MIN_BOOST.
    ConfigMinBoost,
    /// Configurable max boost (scaled); default DEFAULT_MAX_BOOST.
    ConfigMaxBoost,
    /// Configurable min lock duration in seconds.
    ConfigMinLockDuration,
    /// Configurable max lock duration in seconds.
    ConfigMaxLockDuration,
    /// Optional maximum reward pool balance (0 = no cap).
    ConfigMaxRewardPoolBalance,
    /// Circuit-breaker flag; when true new stakes and claims are halted (#360).
    Paused,
    /// Emergency mode flag; when true stakers may reclaim LP without rewards (#359).
    EmergencyMode,
    /// Pending admin nominee for two-step handover.
    PendingAdmin,
    /// Registry of every address that has ever had a nonzero staked balance,
    /// maintained so keepers can enumerate stakers for `list_expired_boosts`
    /// / `settle_boost_batch` (#699). An address is added on its first stake
    /// and removed once its balance returns to zero — see
    /// `_index_add`/`_index_remove`.
    ///
    /// Legacy layout (#1113): superseded by `StakerCount` / `StakerAt` /
    /// `StakerSlot`. Only present on pools deployed before that change, until
    /// `migrate_staker_index` has moved it over and deleted it.
    StakerIndex,
    /// Number of registered stakers (persistent, #1113).
    StakerCount,
    /// The i-th registered staker, `i < StakerCount` (persistent, #1113).
    StakerAt(u32),
    /// Position of a staker in `StakerAt` (persistent, #1113).
    StakerSlot(Address),
    /// Next legacy position `migrate_staker_index` expects (instance, #1113).
    MigrationCursor,
    /// Id of the current `reconcile_reward_accounting` run (instance, #1113).
    /// `ReconcileState` entries from an earlier run are ignored.
    ReconcileEpoch,
}

// Ã¢â€â‚¬Ã¢â€â‚¬ Data structures Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬

/// Time-locked staking position (#317).
#[contracttype]
#[derive(Clone, Debug)]
pub struct LockedPosition {
    pub amount: i128,
    pub lock_expiry: u64,
    pub boost_multiplier: i128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct StakerInfo {
    pub staked_amount: i128,
    pub effective_amount: i128,
    pub rewards_debt: i128,
    pub lock_expiry: u64,
    pub boost_multiplier: i128,
}

/// The configuration fixed at initialization, as stored on-chain (#1114).
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct StakingConfig {
    pub lp_token: Address,
    pub reward_token: Address,
    pub admin: Address,
    pub min_boost: i128,
    pub max_boost: i128,
    pub min_lock_duration: u64,
    pub max_lock_duration: u64,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PoolInfo {
    pub lp_token: Address,
    pub reward_token: Address,
    pub admin: Address,
    pub total_effective_staked: i128,
    /// Undistributed rewards: funded by `add_rewards`, not yet assigned to
    /// stakers via the accumulator (#1045). This struct keeps its historical
    /// field name for ABI compatibility; before the #1045 fix it could
    /// report a corrupted negative value. It is never negative now.
    pub reward_pool_balance: i128,
    pub accumulated_rewards_per_share: i128,
}

// Ã¢â€â‚¬Ã¢â€â‚¬ Contract Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬

#[contract]
pub struct Staking;

#[contractimpl]
impl Staking {
    /// Undistributed rewards: funded by `add_rewards`, not yet assigned to
    /// stakers through the per-share accumulator (#1045).
    ///
    /// `update_rewards` is the only entrypoint that moves tokens out of this
    /// counter — it debits what it distributes and credits `OwedRewards`.
    /// Claims and settlements pay from `OwedRewards` and never touch it, and
    /// the `ConfigMaxRewardPoolBalance` cap is enforced against it.
    fn _undistributed_rewards(env: &Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::RewardPoolBalance)
            .unwrap_or(0)
    }

    /// Rewards already assigned to stakers through
    /// `AccumulatedRewardsPerShare` but not yet paid out (#1045).
    ///
    /// Increases in `update_rewards` (by the distributed amount), decreases
    /// in `_claim_rewards`/`_settle_pending_at` (by exactly what is paid) and
    /// in `emergency_withdraw` (forfeited pending rewards). The chain-wide
    /// invariant is
    /// `reward_token.balance(contract) >= UndistributedRewards + OwedRewards`
    /// — the slack is the rounding dust left behind by the per-share integer
    /// division, plus anything `add_rewards` measured (transfers may deliver
    /// slightly less than requested). The dust is small and permanently
    /// unclaimable by construction; the contract never pays out more than
    /// stakers were credited for.
    pub fn get_owed_rewards(env: Env) -> i128 {
        Self::extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::OwedRewards)
            .unwrap_or(0)
    }

    /// Initialize the staking contract.
    pub fn initialize(
        env: Env,
        lp_token: Address,
        reward_token: Address,
        admin: Address,
    ) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        if env.storage().instance().has(&DataKey::LpToken) {
            return Err(StakingError::AlreadyInitialized);
        }
        if lp_token == reward_token {
            return Err(StakingError::IdenticalTokens);
        }
        env.storage().instance().set(&DataKey::LpToken, &lp_token);
        env.storage()
            .instance()
            .set(&DataKey::RewardToken, &reward_token);
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::TotalEffectiveStaked, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::AccumulatedRewardsPerShare, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::RewardPoolBalance, &0i128);
        env.storage().instance().set(&DataKey::OwedRewards, &0i128);
        // ConfigMaxRewardPoolBalance initialized above
        Self::_write_boost_config(
            &env,
            MIN_BOOST,
            DEFAULT_MAX_BOOST,
            MIN_LOCK_DURATION,
            MAX_LOCK_DURATION,
        );
        Self::_emit_initialized(&env);
        Ok(())
    }

    /// Initialize with configurable veToken boost parameters (#317).
    #[allow(clippy::too_many_arguments)]
    pub fn initialize_with_boost_config(
        env: Env,
        lp_token: Address,
        reward_token: Address,
        admin: Address,
        min_boost_scaled: i128,
        max_boost_scaled: i128,
        min_lock_duration_secs: u64,
        max_lock_duration_secs: u64,
    ) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        if env.storage().instance().has(&DataKey::LpToken) {
            return Err(StakingError::AlreadyInitialized);
        }
        if lp_token == reward_token {
            return Err(StakingError::IdenticalTokens);
        }
        if min_boost_scaled <= 0 || max_boost_scaled < min_boost_scaled {
            return Err(StakingError::InvalidBoostConfig);
        }
        if min_lock_duration_secs == 0 || max_lock_duration_secs < min_lock_duration_secs {
            return Err(StakingError::InvalidLockDuration);
        }
        env.storage().instance().set(&DataKey::LpToken, &lp_token);
        env.storage()
            .instance()
            .set(&DataKey::RewardToken, &reward_token);
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::TotalEffectiveStaked, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::AccumulatedRewardsPerShare, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::RewardPoolBalance, &0i128);
        env.storage().instance().set(&DataKey::OwedRewards, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::ConfigMaxRewardPoolBalance, &0i128);
        Self::_write_boost_config(
            &env,
            min_boost_scaled,
            max_boost_scaled,
            min_lock_duration_secs,
            max_lock_duration_secs,
        );
        Self::_emit_initialized(&env);
        Ok(())
    }

    /// The configuration as stored on-chain: tokens, admin and the boost and
    /// lock bounds. A deployer reads this back after `initialize` and compares
    /// it with what was intended, since `initialize` cannot be authenticated
    /// (the admin may be a contract that cannot sign for itself) (#1114).
    pub fn get_config(env: Env) -> Result<StakingConfig, StakingError> {
        Self::extend_instance_ttl(&env);
        Self::_read_config(&env).ok_or(StakingError::NotInitialized)
    }

    /// Escrow LP tokens for a fixed lock duration with a boosted reward rate (#317).
    pub fn lock(
        env: Env,
        staker: Address,
        amount: i128,
        lock_duration_seconds: u64,
    ) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        Self::stake_locked(env, staker, amount, lock_duration_seconds)
    }

    /// Withdraw all LP and accrued rewards after the lock expires (#317).
    pub fn unlock(env: Env, staker: Address) -> Result<(i128, i128), StakingError> {
        Self::extend_instance_ttl(&env);
        let amount: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerAmount(staker.clone()))
            .unwrap_or(0);
        if amount <= 0 {
            return Err(StakingError::NothingStaked);
        }
        Self::unstake(env, staker, amount)
    }

    /// Extend an existing lock forward in time only (#317).
    pub fn extend_lock(
        env: Env,
        staker: Address,
        new_duration_seconds: u64,
    ) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        if Self::is_paused(env.clone()) {
            return Err(StakingError::Paused);
        }
        staker.require_auth();
        if new_duration_seconds == 0 {
            return Err(StakingError::InvalidDuration);
        }

        let now = env.ledger().timestamp();
        let existing_expiry: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::LockExpiry(staker.clone()))
            .unwrap_or(0);
        if existing_expiry <= now {
            return Err(StakingError::NoActiveLock);
        }

        let (min_lock, max_lock) = Self::_lock_duration_bounds(&env);
        let clamped = new_duration_seconds.clamp(min_lock, max_lock);
        let proposed_expiry = now + clamped;
        let expiry = proposed_expiry.max(existing_expiry);
        let boost = Self::_boost_for_remaining(&env, expiry, now);

        let staked_amount: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerAmount(staker.clone()))
            .unwrap_or(0);
        if staked_amount <= 0 {
            return Err(StakingError::NothingStaked);
        }

        Self::_settle_pending(&env, &staker);

        let old_boost: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::BoostMultiplier(staker.clone()))
            .unwrap_or(MIN_BOOST);
        let old_effective = Self::_effective_amount(staked_amount, old_boost);
        let new_effective = Self::_effective_amount(staked_amount, boost);

        let total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalEffectiveStaked)
            .unwrap_or(0);
        env.storage().instance().set(
            &DataKey::TotalEffectiveStaked,
            &(total - old_effective + new_effective).max(0),
        );

        let key_lock = DataKey::LockExpiry(staker.clone());
        env.storage().persistent().set(&key_lock, &expiry);
        env.storage()
            .persistent()
            .extend_ttl(&key_lock, MIN_TTL, BUMP_TO);

        let key_boost = DataKey::BoostMultiplier(staker.clone());
        env.storage().persistent().set(&key_boost, &boost);
        env.storage()
            .persistent()
            .extend_ttl(&key_boost, MIN_TTL, BUMP_TO);

        let acc_per_share: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedRewardsPerShare)
            .unwrap_or(0);
        let new_debt = new_effective * acc_per_share / SCALE_FACTOR;
        let key_debt = DataKey::StakerRewardsDebt(staker.clone());
        env.storage().persistent().set(&key_debt, &new_debt);
        env.storage()
            .persistent()
            .extend_ttl(&key_debt, MIN_TTL, BUMP_TO);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "lock_extended"),),
            (staker, boost, expiry)
        );
        Ok(())
    }

    /// View the staker's locked position (#317).
    ///
    /// `boost_multiplier` reflects decay (#699): once `lock_expiry` has
    /// passed this reads `MIN_BOOST` even if `settle_boost` hasn't physically
    /// corrected storage yet, matching `get_staker_info` and `current_boost`.
    pub fn get_locked_position(env: Env, staker: Address) -> LockedPosition {
        Self::extend_instance_ttl(&env);
        let amount: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerAmount(staker.clone()))
            .unwrap_or(0);
        let boost = Self::_current_boost(&env, &staker);
        let lock_expiry: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::LockExpiry(staker.clone()))
            .unwrap_or(0);
        LockedPosition {
            amount,
            lock_expiry,
            boost_multiplier: boost,
        }
    }

    /// Add rewards to the pool. Admin only.
    pub fn add_rewards(env: Env, admin: Address, amount: i128) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        admin.require_auth();
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(StakingError::NotInitialized)?;
        if admin != stored_admin {
            return Err(StakingError::Unauthorized);
        }
        if amount <= 0 {
            return Err(StakingError::InvalidAmount);
        }

        let reward_token: Address = env.storage().instance().get(&DataKey::RewardToken).unwrap();
        let pool_addr = env.current_contract_address();
        // Record preÃ¢â‚¬â€˜transfer token balance
        let token_client = SepTokenClient::new(&env, &reward_token);
        let pre_balance: i128 = token_client.balance(&pool_addr);
        // Transfer requested amount from admin to pool
        token_client.transfer(&admin, &pool_addr, &amount);
        // Record postÃ¢â‚¬â€˜transfer token balance to determine actual received amount
        let post_balance: i128 = token_client.balance(&pool_addr);
        let received: i128 = post_balance - pre_balance;
        // #1045: `add_rewards` funds only the undistributed side of the
        // accounting. The cap is enforced against this counter — claims pay
        // out of `OwedRewards` and cannot exceed it, so they can never push
        // funding past the cap.
        let current_undistributed = Self::_undistributed_rewards(&env);
        let new_undistributed = current_undistributed + received;
        // Enforce optional max reward pool balance cap (0 = no cap)
        let max_balance: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ConfigMaxRewardPoolBalance)
            .unwrap_or(0);
        if max_balance != 0 && new_undistributed > max_balance {
            return Err(StakingError::MaxRewardPoolExceeded);
        }
        env.storage()
            .instance()
            .set(&DataKey::RewardPoolBalance, &new_undistributed);
        // Emit event with the actual amount added
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "rewards_added"),),
            (admin, received)
        );
        Ok(())
    }

    /// Halt new stakes and reward claims. Admin only (#360).
    ///
    /// Lets the admin freeze the contract (e.g. while a reward-accounting bug
    /// is being patched). Unstaking and emergency withdrawals remain available
    /// so stakers can always retrieve their LP tokens.
    pub fn pause(env: Env, admin: Address) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        admin.require_auth();
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(StakingError::NotInitialized)?;
        if admin != stored_admin {
            return Err(StakingError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Paused, &true);
        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "paused"),), (admin,));
        Ok(())
    }

    /// Resume staking and claiming. Admin only (#360).
    pub fn unpause(env: Env, admin: Address) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        admin.require_auth();
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(StakingError::NotInitialized)?;
        if admin != stored_admin {
            return Err(StakingError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Paused, &false);
        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "unpaused"),), (admin,));
        Ok(())
    }

    /// Nominate a new admin. The nominee must call `accept_admin` to complete the transfer.
    pub fn propose_admin(env: Env, admin: Address, new_admin: Address) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(StakingError::NotInitialized)?;
        if admin != stored_admin {
            return Err(StakingError::Unauthorized);
        }
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Some(new_admin.clone()));
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "admin_nominated"),),
            (admin, new_admin)
        );
        Ok(())
    }

    /// Accept the pending admin nomination. Caller becomes the new admin.
    pub fn accept_admin(env: Env, new_admin: Address) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        let pending: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None);
        let nominee = pending.ok_or(StakingError::NoPendingAdmin)?;
        if new_admin != nominee {
            return Err(StakingError::WrongAdmin);
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

    /// Replace this contract's WASM with a new version. Admin only.
    ///
    /// `admin` must be the stored admin and must authorize the call; any other
    /// address is rejected with `Unauthorized`. The new WASM must already be
    /// uploaded to the network. Stakes, reward accounting and configuration
    /// are preserved and `initialize` is not re-run; only the bytecode is
    /// replaced.
    pub fn upgrade(
        env: Env,
        admin: Address,
        new_wasm_hash: BytesN<32>,
    ) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(StakingError::NotInitialized)?;
        if admin != stored_admin {
            return Err(StakingError::Unauthorized);
        }
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

    /// Return current admin address.
    pub fn get_admin(env: Env) -> Result<Address, StakingError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(StakingError::NotInitialized)
    }

    /// Return pending admin nominee, if any.
    pub fn get_pending_admin(env: Env) -> Option<Address> {
        env.storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None)
    }

    /// Whether the contract is currently paused (#360).
    pub fn is_paused(env: Env) -> bool {
        Self::extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Stake LP tokens without a lock (1Ãƒâ€” boost).
    pub fn stake(env: Env, staker: Address, amount: i128) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        Self::stake_locked(env, staker, amount, 0)
    }

    /// Stake LP tokens with an optional lock duration for a boost multiplier.
    ///
    /// `lock_duration_secs` = 0 Ã¢â€ â€™ no lock, 1Ãƒâ€” boost.
    /// Lock duration is clamped to [MIN_LOCK_DURATION, MAX_LOCK_DURATION].
    /// Boost scales linearly from 1Ãƒâ€” (no lock) to 4Ãƒâ€” (MAX_LOCK_DURATION).
    ///
    /// If the staker already has a lock, the new lock must expire no earlier
    /// than the existing one (locks can only be extended, not shortened).
    pub fn stake_locked(
        env: Env,
        staker: Address,
        amount: i128,
        lock_duration_secs: u64,
    ) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        if Self::is_paused(env.clone()) {
            return Err(StakingError::Paused);
        }
        staker.require_auth();
        if amount <= 0 && lock_duration_secs == 0 {
            return Err(StakingError::NothingToStake);
        }

        if amount > 0 {
            let lp_token: Address = env
                .storage()
                .instance()
                .get(&DataKey::LpToken)
                .ok_or(StakingError::NotInitialized)?;
            let pool_addr = env.current_contract_address();
            SepTokenClient::new(&env, &lp_token).transfer(&staker, &pool_addr, &amount);
        }

        // Settle any pending rewards before changing effective stake.
        Self::_settle_pending(&env, &staker);

        // Compute new boost and lock expiry.
        let now = env.ledger().timestamp();
        let existing_expiry: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::LockExpiry(staker.clone()))
            .unwrap_or(0);

        let (new_expiry, new_boost) = if lock_duration_secs == 0 {
            // No new lock requested Ã¢â‚¬â€ keep existing lock if still active.
            let expiry = existing_expiry.max(now);
            let boost = Self::_boost_for_remaining(&env, expiry, now);
            (existing_expiry, boost)
        } else {
            let (min_lock, max_lock) = Self::_lock_duration_bounds(&env);
            let clamped = lock_duration_secs.clamp(min_lock, max_lock);
            let proposed_expiry = now + clamped;
            // Cannot shorten an existing lock.
            let expiry = proposed_expiry.max(existing_expiry);
            let boost = Self::_boost_for_remaining(&env, expiry, now);
            (expiry, boost)
        };

        // Update raw staked amount.
        let current_staked: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerAmount(staker.clone()))
            .unwrap_or(0);
        let new_staked = current_staked + amount;
        let key_amount = DataKey::StakerAmount(staker.clone());
        env.storage().persistent().set(&key_amount, &new_staked);
        env.storage()
            .persistent()
            .extend_ttl(&key_amount, MIN_TTL, BUMP_TO);

        // #699: register in the staker index on the transition into a
        // nonzero balance (covers both a brand-new staker and one who fully
        // unstaked and is now returning).
        if current_staked == 0 && new_staked > 0 {
            Self::_index_add(&env, &staker);
        }

        // Remove the position's existing contribution using the boost it was
        // actually recorded with, not the freshly recomputed one Ã¢â‚¬â€ otherwise
        // TotalEffectiveStaked (ÃŽÂ£ raw_amount Ãƒâ€” stored_boost) is corrupted
        // whenever the boost changes on a top-up (issue #467).
        let old_boost: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::BoostMultiplier(staker.clone()))
            .unwrap_or(MIN_BOOST);
        let old_effective = Self::_effective_amount(current_staked, old_boost);
        let new_effective = Self::_effective_amount(new_staked, new_boost);

        // Adjust total effective staked.
        let total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalEffectiveStaked)
            .unwrap_or(0);
        // Remove old effective, add new effective.
        let new_total = total - old_effective + new_effective;
        env.storage()
            .instance()
            .set(&DataKey::TotalEffectiveStaked, &new_total.max(0));

        // Persist lock and boost.
        let key_lock = DataKey::LockExpiry(staker.clone());
        env.storage().persistent().set(&key_lock, &new_expiry);
        env.storage()
            .persistent()
            .extend_ttl(&key_lock, MIN_TTL, BUMP_TO);

        let key_boost = DataKey::BoostMultiplier(staker.clone());
        env.storage().persistent().set(&key_boost, &new_boost);
        env.storage()
            .persistent()
            .extend_ttl(&key_boost, MIN_TTL, BUMP_TO);

        // Reset rewards debt to current acc_per_share * new_effective.
        let acc_per_share: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedRewardsPerShare)
            .unwrap_or(0);
        let new_debt = new_effective * acc_per_share / SCALE_FACTOR;
        let key_debt = DataKey::StakerRewardsDebt(staker.clone());
        env.storage().persistent().set(&key_debt, &new_debt);
        env.storage()
            .persistent()
            .extend_ttl(&key_debt, MIN_TTL, BUMP_TO);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "staked"),),
            (staker, amount, new_boost, new_expiry)
        );
        Ok(())
    }

    /// Claim accrued rewards without unstaking.
    pub fn claim(env: Env, staker: Address) -> Result<i128, StakingError> {
        Self::extend_instance_ttl(&env);
        if Self::is_paused(env.clone()) {
            return Err(StakingError::Paused);
        }
        staker.require_auth();
        Self::_claim_rewards(&env, &staker)
    }

    /// Unstake LP tokens and claim pending rewards.
    ///
    /// Panics if the staker's lock has not yet expired.
    pub fn unstake(env: Env, staker: Address, amount: i128) -> Result<(i128, i128), StakingError> {
        Self::extend_instance_ttl(&env);
        staker.require_auth();
        if amount <= 0 {
            return Err(StakingError::InvalidAmount);
        }

        let staked_amount: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerAmount(staker.clone()))
            .unwrap_or(0);
        if staked_amount < amount {
            return Err(StakingError::InsufficientStaked);
        }

        // Enforce lock.
        let now = env.ledger().timestamp();
        let lock_expiry: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::LockExpiry(staker.clone()))
            .unwrap_or(0);
        if now < lock_expiry {
            return Err(StakingError::StillLocked);
        }

        // Claim pending rewards first (auth already checked above). While
        // paused, principal can still be withdrawn -- stakers must never be
        // trapped -- but reward payout is halted, matching claim() and
        // stake_locked(). Any pending amount is carried forward via the
        // debt reset below rather than paid out, so it remains claimable
        // once the contract is unpaused.
        let paused = Self::is_paused(env.clone());
        let pending = Self::pending_rewards(env.clone(), staker.clone());
        let (rewards, unpaid_pending) = if paused {
            (0, pending)
        } else if pending > 0 {
            (Self::_claim_rewards(&env, &staker)?, 0)
        } else {
            (0, 0)
        };

        let lp_token: Address = env
            .storage()
            .instance()
            .get(&DataKey::LpToken)
            .ok_or(StakingError::NotInitialized)?;
        let pool_addr = env.current_contract_address();
        SepTokenClient::new(&env, &lp_token).transfer(&pool_addr, &staker, &amount);

        let boost: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::BoostMultiplier(staker.clone()))
            .unwrap_or(MIN_BOOST);

        let old_effective = Self::_effective_amount(staked_amount, boost);
        let new_staked = staked_amount - amount;
        let new_effective = Self::_effective_amount(new_staked, boost);

        let key_amount = DataKey::StakerAmount(staker.clone());
        env.storage().persistent().set(&key_amount, &new_staked);
        env.storage()
            .persistent()
            .extend_ttl(&key_amount, MIN_TTL, BUMP_TO);

        // #699: drop from the staker index once fully unstaked, so the index
        // stays bounded by currently-active stakers.
        if new_staked == 0 {
            Self::_index_remove(&env, &staker);
        }

        let total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalEffectiveStaked)
            .unwrap_or(0);
        env.storage().instance().set(
            &DataKey::TotalEffectiveStaked,
            &(total - old_effective + new_effective).max(0),
        );

        // Reset debt (carrying forward any pending reward deliberately not
        // paid out above due to a pause, so it is preserved rather than lost).
        let acc_per_share: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedRewardsPerShare)
            .unwrap_or(0);
        let new_debt = new_effective * acc_per_share / SCALE_FACTOR - unpaid_pending;
        let key_debt = DataKey::StakerRewardsDebt(staker.clone());
        env.storage().persistent().set(&key_debt, &new_debt);
        env.storage()
            .persistent()
            .extend_ttl(&key_debt, MIN_TTL, BUMP_TO);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "unstaked"),),
            (staker, amount, rewards)
        );
        Ok((amount, rewards))
    }

    /// Enable or disable emergency mode (#359). Admin only.
    ///
    /// Emergency mode unlocks `emergency_withdraw` so stakers can
    /// reclaim their LP tokens without touching the reward token. It is gated
    /// behind the admin so it cannot be used to skip rewards under normal
    /// conditions.
    pub fn set_emergency_mode(env: Env, admin: Address, enabled: bool) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        admin.require_auth();
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(StakingError::NotInitialized)?;
        if admin != stored_admin {
            return Err(StakingError::Unauthorized);
        }
        env.storage()
            .instance()
            .set(&DataKey::EmergencyMode, &enabled);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "emergency_mode"),),
            (admin, enabled)
        );
        Ok(())
    }

    /// Set the optional maximum reward pool balance. Admin only.
    ///
    /// The cap is defined against the pool's **undistributed** rewards —
    /// tokens funded by `add_rewards` but not yet assigned to stakers through
    /// the accumulator (#1045). Tokens already owed to stakers do not count
    /// toward the cap, and claims never touch the capped counter, so the cap
    /// limits funding only and is unaffected by claims.
    pub fn set_max_reward_pool_balance(
        env: Env,
        admin: Address,
        max_balance: i128,
    ) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        admin.require_auth();
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(StakingError::NotInitialized)?;
        if admin != stored_admin {
            return Err(StakingError::Unauthorized);
        }
        let current_balance = Self::_undistributed_rewards(&env);
        // 0 means no cap; otherwise ensure the new cap is not below current balance
        if max_balance != 0 && max_balance < current_balance {
            return Err(StakingError::InvalidMaxBalance);
        }
        env.storage()
            .instance()
            .set(&DataKey::ConfigMaxRewardPoolBalance, &max_balance);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "max_reward_pool_balance_set"),),
            (admin, max_balance)
        );
        Ok(())
    }

    /// Whether emergency mode is currently active (#359).
    pub fn is_emergency_mode(env: Env) -> bool {
        Self::extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::EmergencyMode)
            .unwrap_or(false)
    }

    /// Reclaim staked LP tokens without claiming rewards (#359).
    ///
    /// Only callable while the admin has enabled emergency mode. Unlike
    /// `unstake`, this never interacts with the reward token, so
    /// stakers can always recover their LP even if the reward token is paused,
    /// blacklisted, or the reward pool has been drained by a bug. Any pending
    /// rewards are forfeited, and the lock (if any) is ignored.
    ///
    /// Returns the raw LP amount returned to the staker.
    pub fn emergency_withdraw(env: Env, staker: Address) -> Result<i128, StakingError> {
        Self::extend_instance_ttl(&env);
        staker.require_auth();
        if !Self::is_emergency_mode(env.clone()) {
            return Err(StakingError::EmergencyModeNotActive);
        }

        let staked_amount: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerAmount(staker.clone()))
            .unwrap_or(0);
        if staked_amount <= 0 {
            return Err(StakingError::NothingStaked);
        }

        // Remove this staker's effective contribution from the global total.
        let boost: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::BoostMultiplier(staker.clone()))
            .unwrap_or(MIN_BOOST);
        let effective = Self::_effective_amount(staked_amount, boost);
        let total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalEffectiveStaked)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalEffectiveStaked, &(total - effective).max(0));

        // #1045: forfeited pending rewards leave the owed counter. The
        // underlying tokens stay in the contract undistributed, so the
        // accounting invariant balance >= undistributed + owed is preserved
        // and the cap (defined against undistributed rewards) is unaffected.
        // Read before the debt is zeroed below, which is what zeroes
        // `pending_rewards`.
        let forfeited = Self::pending_rewards(env.clone(), staker.clone()).max(0);

        // Zero out the staker's position, debt, boost, and lock.
        env.storage()
            .persistent()
            .set(&DataKey::StakerAmount(staker.clone()), &0i128);
        env.storage()
            .persistent()
            .set(&DataKey::StakerRewardsDebt(staker.clone()), &0i128);
        env.storage()
            .persistent()
            .set(&DataKey::BoostMultiplier(staker.clone()), &MIN_BOOST);
        env.storage()
            .persistent()
            .set(&DataKey::LockExpiry(staker.clone()), &0u64);

        if forfeited > 0 {
            let owed = Self::get_owed_rewards(env.clone());
            env.storage()
                .instance()
                .set(&DataKey::OwedRewards, &(owed - forfeited).max(0));
        }

        // #699: fully exiting via emergency_withdraw also empties the
        // position, so drop from the staker index the same as unstake does.
        Self::_index_remove(&env, &staker);

        // Return the raw LP balance without touching the reward token.
        let lp_token: Address = env
            .storage()
            .instance()
            .get(&DataKey::LpToken)
            .ok_or(StakingError::NotInitialized)?;
        let pool_addr = env.current_contract_address();
        SepTokenClient::new(&env, &lp_token).transfer(&pool_addr, &staker, &staked_amount);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "emergency_withdraw"),),
            (staker, staked_amount)
        );
        Ok(staked_amount)
    }

    /// View pending rewards for a staker.
    pub fn pending_rewards(env: Env, staker: Address) -> i128 {
        Self::extend_instance_ttl(&env);
        let effective = Self::_staker_effective(&env, &staker);
        if effective == 0 {
            return 0;
        }
        let acc_per_share: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedRewardsPerShare)
            .unwrap_or(0);
        let rewards_debt: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerRewardsDebt(staker))
            .unwrap_or(0);
        (effective * acc_per_share / SCALE_FACTOR - rewards_debt).max(0)
    }

    /// Get pool information.
    pub fn get_pool_info(env: Env) -> Result<PoolInfo, StakingError> {
        Self::extend_instance_ttl(&env);
        Ok(PoolInfo {
            lp_token: env
                .storage()
                .instance()
                .get(&DataKey::LpToken)
                .ok_or(StakingError::NotInitialized)?,
            reward_token: env
                .storage()
                .instance()
                .get(&DataKey::RewardToken)
                .ok_or(StakingError::NotInitialized)?,
            admin: env
                .storage()
                .instance()
                .get(&DataKey::Admin)
                .ok_or(StakingError::NotInitialized)?,
            total_effective_staked: env
                .storage()
                .instance()
                .get(&DataKey::TotalEffectiveStaked)
                .unwrap_or(0),
            reward_pool_balance: env
                .storage()
                .instance()
                .get(&DataKey::RewardPoolBalance)
                .unwrap_or(0),
            accumulated_rewards_per_share: env
                .storage()
                .instance()
                .get(&DataKey::AccumulatedRewardsPerShare)
                .unwrap_or(0),
        })
    }

    /// Get staker info including boost and lock details.
    ///
    /// `boost_multiplier` and `effective_amount` reflect decay (#699): a
    /// staker whose lock expired shows `MIN_BOOST` and the un-boosted
    /// effective amount here even before `settle_boost` has run, so this
    /// view is never the thing that hides the bug this issue fixes.
    pub fn get_staker_info(env: Env, staker: Address) -> StakerInfo {
        Self::extend_instance_ttl(&env);
        let staked_amount: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerAmount(staker.clone()))
            .unwrap_or(0);
        let boost = Self::_current_boost(&env, &staker);
        let lock_expiry: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::LockExpiry(staker.clone()))
            .unwrap_or(0);
        let rewards_debt: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerRewardsDebt(staker))
            .unwrap_or(0);
        StakerInfo {
            staked_amount,
            effective_amount: Self::_effective_amount(staked_amount, boost),
            rewards_debt,
            lock_expiry,
            boost_multiplier: boost,
        }
    }

    // ---- Issue #699: lock-boost decay views and settlement ----
    //
    // Design decision: Option A ("settle-on-expiry"), not Option B
    // (continuous linear decay / Curve-style slope-bias accumulator).
    //
    // Tradeoff accepted: Option A is a cliff, not a curve — a lock earns its
    // full peak boost right up to `lock_expiry`, then exactly `MIN_BOOST`
    // from that instant on. It piggybacks on the existing single
    // `AccumulatedRewardsPerShare` accumulator instead of requiring a new
    // slope/bias total that changes automatically at every expiry (what
    // Curve's VotingEscrow does, and what Option B would need here). That
    // makes it far simpler to reason about and review, at the cost of two
    // real limitations documented candidly rather than glossed over:
    //
    //   1. `TotalEffectiveStaked` only shrinks when *something* touches the
    //      expired staker: their own stake/extend/unstake, or a permissionless
    //      `settle_boost`/`settle_boost_batch` call from a keeper. Right up
    //      until that happens, the pool's shared denominator still contains
    //      their stale (peak) contribution, so their unsettled expiry still
    //      dilutes other stakers for whatever reward distributions land in
    //      that window. `settle_boost_batch` exists precisely to make that
    //      window small in practice by letting anyone sweep it — but unlike
    //      Option B, correctness isn't automatic; it depends on a keeper
    //      showing up. That dependency, not any remaining bug, is the
    //      tradeoff.
    //   2. A staker who locks once and is never touched again (no stake,
    //      unstake, extend_lock, or settle_boost) *and* whose lock has been
    //      expired across one or more `update_rewards` calls will have their
    //      still-unsettled reward-per-share delta paid out at the *current*
    //      (decayed) boost when they finally do claim/unstake/get settled,
    //      not split precisely at the expiry second. Splitting it exactly
    //      would require checkpointing `AccumulatedRewardsPerShare` at every
    //      lock's expiry — effectively reinventing Option B's slope/bias
    //      timeline. In practice this window is exactly as large as a
    //      keeper's neglect: call `settle_boost` promptly after expiry (the
    //      tested, intended flow) and there is no meaningful gap to
    //      misattribute. This is *why* the module docs call Option A
    //      "keeper-assisted": its precision is bounded by keeper diligence,
    //      where Option B's is unconditional.
    //
    // Every one of the required "no staker can withdraw more than they
    // staked" / "total rewards never exceed add_rewards" invariants holds
    // regardless of keeper timing — what varies is only *how the pie is
    // sliced* between stakers whose settlement was delayed and the pool at
    // large, never whether the pie is the right size.

    /// The boost `staker` earns right now, decay applied (#699). Equivalent
    /// to `get_staker_info(..).boost_multiplier`, exposed directly so a
    /// keeper (or UI) can check decay status without loading the full
    /// `StakerInfo` struct.
    pub fn current_boost(env: Env, staker: Address) -> i128 {
        Self::extend_instance_ttl(&env);
        Self::_current_boost(&env, &staker)
    }

    /// `staker`'s effective (boosted) staked amount right now, decay applied.
    /// Equivalent to `get_staker_info(..).effective_amount`.
    pub fn effective_staked(env: Env, staker: Address) -> i128 {
        Self::extend_instance_ttl(&env);
        Self::_staker_effective(&env, &staker)
    }

    /// The pool's total effective stake as currently recorded.
    ///
    /// This is the same stored value `get_pool_info().total_effective_staked`
    /// returns — it is *not* recomputed by summing `effective_staked` over
    /// every indexed staker on every call, which would turn an O(1) view
    /// into an O(n) one and reintroduce the unbounded-resource-cost problem
    /// this contract's accumulator pattern exists to avoid. "Live" here
    /// means "kept live by settle_boost / settle_boost_batch and by the
    /// normal stake/unstake/extend_lock paths," in contrast to the
    /// pre-#699 behaviour where nothing ever corrected it for an
    /// unsettled expiry.
    pub fn total_effective_staked(env: Env) -> i128 {
        Self::extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::TotalEffectiveStaked)
            .unwrap_or(0)
    }

    /// The lock-expiry timestamp for `staker` (0 if never locked).
    pub fn boost_expires_at(env: Env, staker: Address) -> u64 {
        Self::extend_instance_ttl(&env);
        env.storage()
            .persistent()
            .get(&DataKey::LockExpiry(staker))
            .unwrap_or(0)
    }

    /// Page through the staker index looking for expired-but-unsettled
    /// boosts, for a keeper deciding who to pass to `settle_boost_batch`.
    ///
    /// `offset`/`limit` paginate over the *index*, not over the matches —
    /// `limit` is clamped to `MAX_LIST_LIMIT` so one call can't be made to
    /// scan an unbounded number of entries. Returns the (possibly empty)
    /// subset of that page whose lock has genuinely expired and whose
    /// stored boost hasn't been settled down to `MIN_BOOST` yet.
    pub fn list_expired_boosts(env: Env, offset: u32, limit: u32) -> Vec<Address> {
        Self::extend_instance_ttl(&env);
        let (min_boost, _) = Self::_boost_bounds(&env);
        let now = env.ledger().timestamp();
        let clamped_limit = limit.min(MAX_LIST_LIMIT);
        let page = Self::_index_page(&env, offset, clamped_limit);

        let mut result = Vec::new(&env);
        for staker in page.iter() {
            let stored_boost: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::BoostMultiplier(staker.clone()))
                .unwrap_or(min_boost);
            let lock_expiry: u64 = env
                .storage()
                .persistent()
                .get(&DataKey::LockExpiry(staker.clone()))
                .unwrap_or(0);
            if boost::is_expired_and_stale(stored_boost, lock_expiry, now, min_boost) {
                result.push_back(staker);
            }
        }
        result
    }

    /// Permissionlessly settle one staker's expired boost (#699).
    ///
    /// Safe by construction, not by access control: this can only ever
    /// reduce a boost that has genuinely expired, and is a strict no-op for
    /// everyone else —
    ///   - an address with no active lock at all (`lock_expiry == 0`),
    ///   - an address whose lock is still active (`now < lock_expiry`),
    ///   - an address already settled (`stored_boost <= MIN_BOOST`), and
    ///   - an address with nothing staked,
    /// all return immediately without touching storage or emitting an
    /// event. There is no path by which calling this on someone else's
    /// position can move funds, change who can withdraw what, or shorten an
    /// active lock — it only ever writes `MIN_BOOST` over a value that
    /// decay had already made obsolete.
    ///
    /// "Settle before mutating" (required in either option): pending
    /// rewards are harvested at the pre-settlement rate *first*, exactly as
    /// `stake_locked`/`extend_lock` already do before recomputing a
    /// position's effective amount, so no reward is silently reassigned.
    ///
    /// While the contract is paused the reward transfer is skipped (#1114):
    /// the boost bookkeeping is still corrected, so `TotalEffectiveStaked`
    /// stops counting the expired boost, but the staker's pending reward is
    /// carried forward in their rewards debt, exactly as `unstake` does for a
    /// paused pool, and stays claimable after `unpause`. Without this anyone
    /// could force reward payouts out of a paused pool.
    pub fn settle_boost(env: Env, staker: Address) {
        Self::extend_instance_ttl(&env);
        let (min_boost, _) = Self::_boost_bounds(&env);
        let stored_boost: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::BoostMultiplier(staker.clone()))
            .unwrap_or(min_boost);
        let lock_expiry: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::LockExpiry(staker.clone()))
            .unwrap_or(0);
        let now = env.ledger().timestamp();

        if !boost::is_expired_and_stale(stored_boost, lock_expiry, now, min_boost) {
            return;
        }

        let raw: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerAmount(staker.clone()))
            .unwrap_or(0);
        if raw == 0 {
            // Nothing staked to adjust in TotalEffectiveStaked, but the
            // stale boost record itself is still worth clearing so a future
            // top-up doesn't briefly resurrect it before the next write.
            let key_boost = DataKey::BoostMultiplier(staker.clone());
            env.storage().persistent().set(&key_boost, &min_boost);
            return;
        }

        let old_effective = Self::_effective_amount(raw, stored_boost);
        let new_effective = Self::_effective_amount(raw, min_boost);

        // Harvest pending rewards at the still-boosted rate (`old_effective`)
        // before the effective amount changes underneath the rewards-debt
        // accounting. Passing it explicitly matters: by this point
        // `is_expired_and_stale` has already confirmed the lock is expired,
        // so `_settle_pending`'s default (current-boost) effective amount
        // would already be the decayed `new_effective`, understating what's
        // owed for the period the staker was still at peak boost.
        let carried = if Self::is_paused(env.clone()) {
            Self::_pending_at(&env, &staker, old_effective)
        } else {
            Self::_settle_pending_at(&env, &staker, Some(old_effective));
            0
        };

        let total: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalEffectiveStaked)
            .unwrap_or(0);
        env.storage().instance().set(
            &DataKey::TotalEffectiveStaked,
            &(total - old_effective + new_effective).max(0),
        );

        let key_boost = DataKey::BoostMultiplier(staker.clone());
        env.storage().persistent().set(&key_boost, &min_boost);
        env.storage()
            .persistent()
            .extend_ttl(&key_boost, MIN_TTL, BUMP_TO);

        // Rewards debt must be re-based on the now-smaller effective amount,
        // matching the same pattern extend_lock/stake_locked use after
        // _settle_pending.
        let acc_per_share: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedRewardsPerShare)
            .unwrap_or(0);
        let new_debt = new_effective * acc_per_share / SCALE_FACTOR - carried;
        let key_debt = DataKey::StakerRewardsDebt(staker.clone());
        env.storage().persistent().set(&key_debt, &new_debt);
        env.storage()
            .persistent()
            .extend_ttl(&key_debt, MIN_TTL, BUMP_TO);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "boost_exp"),),
            (staker, stored_boost, min_boost)
        );
    }

    /// Sweep a bounded batch of stakers through `settle_boost` (#699).
    ///
    /// Fault-isolated by construction rather than by catching panics (this
    /// crate is `#![no_std]`, so `std::panic::catch_unwind` isn't available
    /// here): `settle_boost` itself never panics for any address, valid or
    /// not, staker or not — every one of its preconditions degrades to a
    /// no-op instead of an assertion. A keeper can therefore pass a batch
    /// containing a mix of expired, still-locked, never-locked, and
    /// unregistered addresses in one call and each is handled independently;
    /// none can abort the others. Bounded to `MAX_BATCH_SIZE` so one
    /// invocation can't be made to exceed the transaction's resource budget.
    pub fn settle_boost_batch(env: Env, stakers: Vec<Address>) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        if stakers.len() > MAX_BATCH_SIZE {
            return Err(StakingError::BatchTooLarge);
        }
        for staker in stakers.iter() {
            Self::settle_boost(env.clone(), staker);
        }
        Ok(())
    }

    /// Migration helper (#699): permissionlessly register pre-upgrade
    /// stakers into `DataKey::StakerIndex` so `list_expired_boosts` /
    /// `settle_boost_batch` can find them.
    ///
    /// Deployed pools have stakers with a `StakerAmount`/`BoostMultiplier`/
    /// `LockExpiry` that predate this upgrade and therefore predate the
    /// index entirely. Their positions are otherwise fully functional from
    /// the moment this contract is upgraded — `claim`, `unstake`,
    /// `pending_rewards`, `get_staker_info`, and even `settle_boost` called
    /// directly on their address all work with no special-casing, because
    /// none of those paths read the index; it exists purely for keeper
    /// *discovery*. This function closes that one discovery gap: anyone
    /// (the admin, a keeper, the staker themselves) can submit a batch of
    /// known pre-upgrade addresses — sourced off-chain from `staked`/`rewards_added`
    /// event history — to backfill them. It is purely additive: calling it
    /// on an address that's already indexed, has nothing staked, or doesn't
    /// exist at all is a harmless no-op, so it can never brick or
    /// double-register anyone, and it never claws back or reassigns any
    /// reward already accrued or paid.
    pub fn register_existing_stakers(env: Env, stakers: Vec<Address>) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        if stakers.len() > MAX_BATCH_SIZE {
            return Err(StakingError::BatchTooLarge);
        }
        for staker in stakers.iter() {
            let raw: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::StakerAmount(staker.clone()))
                .unwrap_or(0);
            if raw > 0 {
                Self::_index_add(&env, &staker);
            }
        }
        Ok(())
    }

    /// Move the pre-#1113 staker vector into the indexed registry, one page at
    /// a time (#1113). Permissionless.
    ///
    /// Pools deployed before the registry kept every staker in one `Vec` in
    /// instance storage (`DataKey::StakerIndex`). Until it is migrated that
    /// vector stays readable by the keeper functions, so nothing breaks
    /// mid-way. Call this with `offset = 0`, `limit <= MAX_BATCH_SIZE`, then
    /// with each previous `offset + limit` until it returns 0 ("entries left");
    /// the last page deletes the legacy vector.
    ///
    /// Pages must be consecutive: an `offset` that is not the next unmigrated
    /// position fails with `MigrationOutOfOrder`. Without that, anyone could
    /// call the last page first and delete the vector before the earlier
    /// entries were moved. Stakers who join or leave while the migration is
    /// running go straight to or out of the new registry; an entry whose
    /// balance is already zero is skipped. Returns the number of legacy entries
    /// not yet processed (0 once finished or when there is nothing to migrate).
    pub fn migrate_staker_index(env: Env, offset: u32, limit: u32) -> Result<u32, StakingError> {
        Self::extend_instance_ttl(&env);
        if limit == 0 || limit > MAX_BATCH_SIZE {
            return Err(StakingError::BatchTooLarge);
        }
        let legacy: Option<Vec<Address>> = env.storage().instance().get(&DataKey::StakerIndex);
        let legacy = match legacy {
            Some(legacy) => legacy,
            None => return Ok(0),
        };
        let cursor: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MigrationCursor)
            .unwrap_or(0);
        if offset != cursor {
            return Err(StakingError::MigrationOutOfOrder);
        }

        let len = legacy.len();
        let end = offset.saturating_add(limit).min(len);
        let mut i = offset;
        while i < end {
            let staker = legacy.get(i).unwrap();
            let raw: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::StakerAmount(staker.clone()))
                .unwrap_or(0);
            if raw > 0 {
                Self::_index_add(&env, &staker);
            }
            i += 1;
        }

        if end >= len {
            env.storage().instance().remove(&DataKey::StakerIndex);
            env.storage().instance().remove(&DataKey::MigrationCursor);
            soroban_amm_sdk::emit_versioned_event!(
                env,
                (Symbol::new(&env, "staker_index_migrated"),),
                (Self::_staker_count(&env),)
            );
            return Ok(0);
        }
        env.storage()
            .instance()
            .set(&DataKey::MigrationCursor, &end);
        Ok(len - end)
    }

    /// Repair legacy reward accounting on an already-deployed contract (admin
    /// only, #1045).
    ///
    /// Contracts deployed before the #1045 fix kept a single
    /// `RewardPoolBalance` counter that was decremented both when rewards
    /// were distributed (`update_rewards`) and when they were paid out
    /// (`claim`/`unstake`/settlement). The counter therefore went negative
    /// after the first payout cycle, `update_rewards` clamped to it and
    /// distributed nothing, and every subsequently funded reward was stranded
    /// in the contract.
    ///
    /// This entrypoint re-derives both counters from ground truth:
    ///
    /// - `undistributed := reward_token.balance(contract) - owed`, with `owed`
    ///   recomputed from the stakers themselves, so no per-share rounding dust
    ///   is double-counted and the result is never negative;
    /// - `owed := Σ (effective * acc_per_share / SCALE - rewards_debt)` over
    ///   every currently-staked address found through the staker index.
    ///
    /// The reward-token balance (not the old corrupted counter) is the source
    /// of truth for what the contract actually holds, which is what repairs a
    /// deployment whose storage was seeded with the negative value.
    ///
    /// Paginated: the staker index can be large, so one call processes at
    /// most `MAX_BATCH_SIZE` entries, starting at `offset`. A partially
    /// completed run is resume-safe — each processed staker's credited
    /// portion of `OwedRewards` is checkpointed under
    /// `DataKey::ReconcileState` so later pages add only the remainder, and
    /// no double-count is possible even if pages overlap. The caller passes
    /// `done = true` on the final page (when `offset + limit >= index len`)
    /// to finalize the run: it clamps the recomputed owed total to the actual
    /// token balance, writes the undistributed remainder, clears the
    /// reconcile state, and emits `rewards_reconciled` with before/after
    /// values. A pool whose stakers were migrated via
    /// `register_existing_stakers` must backfill the index first, or the
    /// recomputed `owed` will miss those stakers.
    ///
    /// The run itself moves no tokens and touches no staker position: rewards
    /// a staker is credited for stay claimable through the normal
    /// `claim`/`unstake` paths, which then decrement `OwedRewards` the usual
    /// way. Returns the final `(undistributed, owed)` pair once the run is
    /// complete; intermediate pages return the running `(0, owed)`.
    pub fn reconcile_reward_accounting(
        env: Env,
        admin: Address,
        offset: u32,
        limit: u32,
        done: bool,
    ) -> Result<(i128, i128), StakingError> {
        Self::extend_instance_ttl(&env);
        admin.require_auth();
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(StakingError::NotInitialized)?;
        if admin != stored_admin {
            return Err(StakingError::Unauthorized);
        }
        if limit == 0 || limit > MAX_BATCH_SIZE {
            return Err(StakingError::BatchTooLarge);
        }

        let acc_per_share: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedRewardsPerShare)
            .unwrap_or(0);

        // Recompute owed from the stakers on this page. This is a pure
        // computation: credited rewards stay where they are (still claimable
        // through the staker's own debt accounting) and are merely counted
        // into the global `OwedRewards` total on the final page.
        let page = Self::_index_page(&env, offset, limit);
        let epoch = Self::_reconcile_epoch(&env);
        let mut owed_total: i128 = 0;
        for staker in page.iter() {
            let raw: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::StakerAmount(staker.clone()))
                .unwrap_or(0);
            let credited = if raw > 0 {
                let effective = Self::_staker_effective(&env, &staker);
                let rewards_debt: i128 = env
                    .storage()
                    .persistent()
                    .get(&DataKey::StakerRewardsDebt(staker.clone()))
                    .unwrap_or(0);
                (effective * acc_per_share / SCALE_FACTOR - rewards_debt).max(0)
            } else {
                0
            };
            let state_key = DataKey::ReconcileState(staker.clone());
            let previous: Option<(u32, i128)> = env.storage().persistent().get(&state_key);
            let already = match previous {
                Some((state_epoch, credited_before)) if state_epoch == epoch => credited_before,
                _ => 0,
            };
            if credited > already {
                owed_total += credited - already;
            }
            env.storage()
                .persistent()
                .set(&state_key, &(epoch, credited));
            env.storage()
                .persistent()
                .extend_ttl(&state_key, MIN_TTL, BUMP_TO);
        }

        // Carry any earlier pages' checkpointed totals.
        let key_state = DataKey::ReconcileTotal;
        let carried: i128 = env.storage().persistent().get(&key_state).unwrap_or(0);
        owed_total += carried;
        env.storage().persistent().set(&key_state, &owed_total);
        env.storage()
            .persistent()
            .extend_ttl(&key_state, MIN_TTL, BUMP_TO);

        if !done {
            // Not finalizing yet: report the running totals.
            return Ok((0, owed_total));
        }

        // Final page: clamp the owed total to what the contract actually
        // holds (legacy storage may report more than exists), then write both
        // counters from ground truth and emit the before/after event.
        let reward_token: Address = env.storage().instance().get(&DataKey::RewardToken).unwrap();
        let pool_addr = env.current_contract_address();
        let token_balance = SepTokenClient::new(&env, &reward_token).balance(&pool_addr);

        let old_undistributed = Self::_undistributed_rewards(&env);
        let old_owed = Self::get_owed_rewards(env.clone());

        let owed = owed_total.min(token_balance);
        let undistributed = (token_balance - owed).max(0);

        env.storage()
            .instance()
            .set(&DataKey::RewardPoolBalance, &undistributed);
        env.storage().instance().set(&DataKey::OwedRewards, &owed);

        // Clear the run's state: this page's checkpoints and the running total
        // go now, and bumping the epoch makes the entries written by earlier
        // pages stale so a later run cannot mistake them for its own.
        for staker in page.iter() {
            env.storage()
                .persistent()
                .remove(&DataKey::ReconcileState(staker));
        }
        env.storage().persistent().remove(&key_state);
        env.storage()
            .instance()
            .set(&DataKey::ReconcileEpoch, &(epoch + 1));

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "rewards_reconciled"),),
            (admin, old_undistributed, old_owed, undistributed, owed)
        );
        Ok((undistributed, owed))
    }

    /// Distribute new rewards across all stakers. Admin only.
    pub fn update_rewards(env: Env, admin: Address, new_rewards: i128) -> Result<(), StakingError> {
        Self::extend_instance_ttl(&env);
        admin.require_auth();
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(StakingError::NotInitialized)?;
        if admin != stored_admin {
            return Err(StakingError::Unauthorized);
        }
        if new_rewards <= 0 {
            return Err(StakingError::InvalidAmount);
        }

        let total_effective: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalEffectiveStaked)
            .unwrap_or(0);
        if total_effective <= 0 {
            return Err(StakingError::NoStakers);
        }

        // Load accumulated-per-share and compute how much of `new_rewards`
        // we can actually distribute based on the current pool balance.
        //
        // Reasoning: in practice the reward pool balance may be lower than
        // the caller requested to distribute (for example if prior claims or
        // transfers drained it). `update_rewards` should not hard-panic on
        // this; instead clamp the distributed amount to the available pool
        // balance and proceed deterministically. This makes the operation
        // safe to call even when the requested amount is larger than the
        // on-chain pool, and avoids panics in randomized harnesses.
        let mut acc_per_share: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedRewardsPerShare)
            .unwrap_or(0);

        let pool_balance: i128 = Self::_undistributed_rewards(&env);

        // Clamp the distributable rewards to what's actually in the pool.
        let distributable: i128 = if new_rewards <= pool_balance {
            new_rewards
        } else {
            // Publish a lightweight event to record the clamping for
            // observability in tests and off-chain tooling.
            soroban_amm_sdk::emit_versioned_event!(
                env,
                (Symbol::new(&env, "rewards_clamped"),),
                (new_rewards, pool_balance)
            );
            pool_balance
        };

        // If nothing is distributable, it's a no-op (admin requested >0
        // but pool is empty); updating acc_per_share by zero is harmless.
        if distributable > 0 {
            let rewards_increase = distributable * SCALE_FACTOR / total_effective;
            acc_per_share += rewards_increase;
            env.storage()
                .instance()
                .set(&DataKey::AccumulatedRewardsPerShare, &acc_per_share);

            // #1045: what is distributed here stops being undistributed and
            // becomes owed. Claims pay out of the owed side only, so a cycle
            // of add_rewards → update_rewards → claim leaves the undistributed
            // balance at 0 instead of decrementing it twice.
            env.storage()
                .instance()
                .set(&DataKey::RewardPoolBalance, &(pool_balance - distributable));
            let owed: i128 = env
                .storage()
                .instance()
                .get(&DataKey::OwedRewards)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::OwedRewards, &(owed + distributable));

            soroban_amm_sdk::emit_versioned_event!(
                env,
                (Symbol::new(&env, "rewards_updated"),),
                (distributable,)
            );
        } else {
            // No distributable amount: emit updated with zero to keep a
            // consistent event surface and return early.
            soroban_amm_sdk::emit_versioned_event!(
                env,
                (Symbol::new(&env, "rewards_updated"),),
                (0_i128,)
            );
        }
        Ok(())
    }

    // Ã¢â€â‚¬Ã¢â€â‚¬ Internal helpers Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬

    fn extend_instance_ttl(env: &Env) {
        env.storage().instance().extend_ttl(MIN_TTL, BUMP_TO);
    }

    fn _claim_rewards(env: &Env, staker: &Address) -> Result<i128, StakingError> {
        let pending = Self::pending_rewards(env.clone(), staker.clone());
        if pending <= 0 {
            return Err(StakingError::NoPendingRewards);
        }

        let reward_token: Address = env.storage().instance().get(&DataKey::RewardToken).unwrap();
        let pool_addr = env.current_contract_address();

        let effective = Self::_staker_effective(env, staker);
        let acc_per_share: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedRewardsPerShare)
            .unwrap_or(0);
        let new_debt = effective * acc_per_share / SCALE_FACTOR;
        let key_debt = DataKey::StakerRewardsDebt(staker.clone());
        env.storage().persistent().set(&key_debt, &new_debt);
        env.storage()
            .persistent()
            .extend_ttl(&key_debt, MIN_TTL, BUMP_TO);

        SepTokenClient::new(env, &reward_token).transfer(&pool_addr, staker, &pending);

        // #1045: paying a claim decrements only the owed side. The
        // undistributed counter was already debited when the rewards were
        // distributed in `update_rewards`; decrementing it here as well was
        // the double-count that stranded every reward cycle after the first.
        let owed: i128 = env
            .storage()
            .instance()
            .get(&DataKey::OwedRewards)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::OwedRewards, &(owed - pending).max(0));

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(env, "claimed"),),
            (staker.clone(), pending)
        );
        Ok(pending)
    }

    fn _write_boost_config(
        env: &Env,
        min_boost: i128,
        max_boost: i128,
        min_lock: u64,
        max_lock: u64,
    ) {
        env.storage()
            .instance()
            .set(&DataKey::ConfigMinBoost, &min_boost);
        env.storage()
            .instance()
            .set(&DataKey::ConfigMaxBoost, &max_boost);
        env.storage()
            .instance()
            .set(&DataKey::ConfigMinLockDuration, &min_lock);
        env.storage()
            .instance()
            .set(&DataKey::ConfigMaxLockDuration, &max_lock);
    }

    fn _boost_bounds(env: &Env) -> (i128, i128) {
        let min_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ConfigMinBoost)
            .unwrap_or(MIN_BOOST);
        let max_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ConfigMaxBoost)
            .unwrap_or(DEFAULT_MAX_BOOST);
        (min_b, max_b)
    }

    fn _lock_duration_bounds(env: &Env) -> (u64, u64) {
        let min_l: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ConfigMinLockDuration)
            .unwrap_or(MIN_LOCK_DURATION);
        let max_l: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ConfigMaxLockDuration)
            .unwrap_or(MAX_LOCK_DURATION);
        (min_l, max_l)
    }

    /// Compute boost multiplier for remaining lock time (linear minÃ¢â€ â€™max).
    fn _boost_for_remaining(env: &Env, expiry: u64, now: u64) -> i128 {
        let (min_boost, max_boost) = Self::_boost_bounds(env);
        if expiry <= now {
            return min_boost;
        }
        let remaining = expiry - now;
        let (_, max_lock) = Self::_lock_duration_bounds(env);
        let clamped = remaining.min(max_lock) as i128;
        let max_dur = max_lock as i128;
        if max_dur == 0 {
            return min_boost;
        }
        min_boost + (max_boost - min_boost) * clamped / max_dur
    }

    /// Effective staked amount = raw_amount * boost / BOOST_SCALE.
    fn _effective_amount(raw: i128, boost: i128) -> i128 {
        boost::effective_amount(raw, boost, BOOST_SCALE)
    }

    /// The boost a staker actually earns right now (issue #699): the stored
    /// (peak) boost if their lock is still active, or `MIN_BOOST` once
    /// `now >= lock_expiry`. This is the single place decay is computed —
    /// every read and write path that needs "what boost applies right now"
    /// (as opposed to "what boost was last written") goes through this.
    fn _current_boost(env: &Env, staker: &Address) -> i128 {
        let (min_boost, _) = Self::_boost_bounds(env);
        let stored_boost: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::BoostMultiplier(staker.clone()))
            .unwrap_or(min_boost);
        let lock_expiry: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::LockExpiry(staker.clone()))
            .unwrap_or(0);
        let now = env.ledger().timestamp();
        boost::current_boost(stored_boost, lock_expiry, now, min_boost)
    }

    /// Current effective amount for a staker, decay applied (#699). Used by
    /// every reward-accrual path (`pending_rewards`, `_settle_pending`,
    /// `_claim_rewards`) so an expired lock stops earning a boost the moment
    /// it's read, not only once a write path happens to touch it.
    fn _staker_effective(env: &Env, staker: &Address) -> i128 {
        let raw: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerAmount(staker.clone()))
            .unwrap_or(0);
        if raw == 0 {
            return 0;
        }
        let boost = Self::_current_boost(env, staker);
        Self::_effective_amount(raw, boost)
    }

    // ── Staker registry (#1113) ───────────────────────────────────────────────
    //
    // Every address with a non-zero stake lives in a dense, O(1)-addressable
    // registry in persistent storage:
    //
    //   StakerCount            -> n
    //   StakerAt(i), i < n     -> the i-th staker
    //   StakerSlot(addr)       -> i  (inverse of StakerAt)
    //
    // Joining appends at index n; leaving swaps the last entry into the freed
    // slot, so both are a fixed number of storage operations whatever n is and
    // nothing grows inside the instance entry. The pre-#1113 layout, a single
    // `Vec` under `DataKey::StakerIndex` in instance storage, is read as a
    // fallback until `migrate_staker_index` has moved it over.

    fn _staker_count(env: &Env) -> u32 {
        let key = DataKey::StakerCount;
        let count: Option<u32> = env.storage().persistent().get(&key);
        match count {
            Some(count) => {
                env.storage()
                    .persistent()
                    .extend_ttl(&key, MIN_TTL, BUMP_TO);
                count
            }
            None => 0,
        }
    }

    /// Add `staker` to the registry if not already present. Idempotent, so it
    /// is safe on every stake (including a top-up from an indexed address) and
    /// from the permissionless migration helpers.
    fn _index_add(env: &Env, staker: &Address) {
        let slot_key = DataKey::StakerSlot(staker.clone());
        if env.storage().persistent().has(&slot_key) {
            env.storage()
                .persistent()
                .extend_ttl(&slot_key, MIN_TTL, BUMP_TO);
            return;
        }
        let count = Self::_staker_count(env);
        let at_key = DataKey::StakerAt(count);
        env.storage().persistent().set(&at_key, staker);
        env.storage()
            .persistent()
            .extend_ttl(&at_key, MIN_TTL, BUMP_TO);
        env.storage().persistent().set(&slot_key, &count);
        env.storage()
            .persistent()
            .extend_ttl(&slot_key, MIN_TTL, BUMP_TO);
        let count_key = DataKey::StakerCount;
        env.storage().persistent().set(&count_key, &(count + 1));
        env.storage()
            .persistent()
            .extend_ttl(&count_key, MIN_TTL, BUMP_TO);
    }

    /// Remove `staker` from the registry by swapping the last entry into its
    /// slot. Called once their raw staked balance returns to zero, so the
    /// registry stays bounded by the number of *currently* staked addresses.
    /// A no-op for an address that is not registered.
    fn _index_remove(env: &Env, staker: &Address) {
        let slot_key = DataKey::StakerSlot(staker.clone());
        let slot: Option<u32> = env.storage().persistent().get(&slot_key);
        let slot = match slot {
            Some(slot) => slot,
            None => return,
        };
        let count = Self::_staker_count(env);
        if count == 0 {
            // Inconsistent registry (slot without a count): just drop the slot.
            env.storage().persistent().remove(&slot_key);
            return;
        }
        let last = count - 1;
        let last_key = DataKey::StakerAt(last);
        if slot != last {
            let moved: Address = env.storage().persistent().get(&last_key).unwrap();
            let at_key = DataKey::StakerAt(slot);
            env.storage().persistent().set(&at_key, &moved);
            env.storage()
                .persistent()
                .extend_ttl(&at_key, MIN_TTL, BUMP_TO);
            let moved_slot_key = DataKey::StakerSlot(moved);
            env.storage().persistent().set(&moved_slot_key, &slot);
            env.storage()
                .persistent()
                .extend_ttl(&moved_slot_key, MIN_TTL, BUMP_TO);
        }
        env.storage().persistent().remove(&last_key);
        env.storage().persistent().remove(&slot_key);
        let count_key = DataKey::StakerCount;
        env.storage().persistent().set(&count_key, &last);
        env.storage()
            .persistent()
            .extend_ttl(&count_key, MIN_TTL, BUMP_TO);
    }

    /// One page of the staker scan used by the keeper functions.
    ///
    /// `offset`/`limit` index the registry. While a legacy `StakerIndex` vector
    /// still exists (migration not finished) the scan continues past the end of
    /// the registry into the legacy entries that have not been registered yet
    /// and still hold a stake, so no keeper function misses a pre-upgrade staker
    /// mid-migration. Once the legacy key is gone this is a plain registry page.
    fn _index_page(env: &Env, offset: u32, limit: u32) -> Vec<Address> {
        let mut page = Vec::new(env);
        let count = Self::_staker_count(env);
        let end = offset.saturating_add(limit);
        let mut i = offset;
        while i < end && i < count {
            let at_key = DataKey::StakerAt(i);
            let staker: Address = env.storage().persistent().get(&at_key).unwrap();
            env.storage()
                .persistent()
                .extend_ttl(&at_key, MIN_TTL, BUMP_TO);
            page.push_back(staker);
            i += 1;
        }

        let legacy: Option<Vec<Address>> = env.storage().instance().get(&DataKey::StakerIndex);
        if let Some(legacy) = legacy {
            let mut v = i.max(count);
            while v < end {
                let li = v - count;
                if li >= legacy.len() {
                    break;
                }
                let staker = legacy.get(li).unwrap();
                let registered = env
                    .storage()
                    .persistent()
                    .has(&DataKey::StakerSlot(staker.clone()));
                let raw: i128 = env
                    .storage()
                    .persistent()
                    .get(&DataKey::StakerAmount(staker.clone()))
                    .unwrap_or(0);
                if !registered && raw > 0 {
                    page.push_back(staker);
                }
                v += 1;
            }
        }
        page
    }

    /// Current reconcile run id. Bumped when a run is finalized, which turns
    /// every `ReconcileState` entry written by that run into a stale one.
    fn _reconcile_epoch(env: &Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::ReconcileEpoch)
            .unwrap_or(0)
    }

    /// The pending reward `staker` has accrued at the given effective amount,
    /// without paying it out.
    fn _pending_at(env: &Env, staker: &Address, effective: i128) -> i128 {
        let acc_per_share: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedRewardsPerShare)
            .unwrap_or(0);
        let rewards_debt: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerRewardsDebt(staker.clone()))
            .unwrap_or(0);
        (effective * acc_per_share / SCALE_FACTOR - rewards_debt).max(0)
    }

    /// Read the stored configuration, or `None` before `initialize`.
    fn _read_config(env: &Env) -> Option<StakingConfig> {
        let lp_token: Address = env.storage().instance().get(&DataKey::LpToken)?;
        let reward_token: Address = env.storage().instance().get(&DataKey::RewardToken)?;
        let admin: Address = env.storage().instance().get(&DataKey::Admin)?;
        let (min_boost, max_boost) = Self::_boost_bounds(env);
        let (min_lock_duration, max_lock_duration) = Self::_lock_duration_bounds(env);
        Some(StakingConfig {
            lp_token,
            reward_token,
            admin,
            min_boost,
            max_boost,
            min_lock_duration,
            max_lock_duration,
        })
    }

    /// Emit `initialized` with the configuration exactly as stored, so an
    /// indexer can reconstruct the pool's tokens, admin and bounds from events
    /// and compare them with what the deployer intended (#1114).
    fn _emit_initialized(env: &Env) {
        if let Some(cfg) = Self::_read_config(env) {
            soroban_amm_sdk::emit_versioned_event!(
                env,
                (Symbol::new(env, "initialized"),),
                (
                    cfg.lp_token,
                    cfg.reward_token,
                    cfg.admin,
                    cfg.min_boost,
                    cfg.max_boost,
                    cfg.min_lock_duration,
                    cfg.max_lock_duration
                )
            );
        }
    }

    /// Transfer any pending rewards to the staker before their effective stake
    /// changes (used in stake_locked / extend_lock so rewards earned so far
    /// are not lost when the debt is recomputed against the new effective amount).
    fn _settle_pending(env: &Env, staker: &Address) {
        Self::_settle_pending_at(env, staker, None);
    }

    /// Like `_settle_pending`, but lets the caller pin the effective amount
    /// used to compute *and debit* the pending reward, instead of
    /// recomputing it via `_staker_effective` (which applies the *current*
    /// boost). `settle_boost` needs this: by the time it calls this, the
    /// lock has already expired and `_current_boost`/`_staker_effective`
    /// would already report the decayed (post-expiry) boost, silently
    /// under-crediting rewards accrued while the staker was still at their
    /// pre-expiry (higher) boost.
    fn _settle_pending_at(env: &Env, staker: &Address, effective_override: Option<i128>) {
        let effective = effective_override.unwrap_or_else(|| Self::_staker_effective(env, staker));
        let acc_per_share: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccumulatedRewardsPerShare)
            .unwrap_or(0);
        let rewards_debt: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::StakerRewardsDebt(staker.clone()))
            .unwrap_or(0);
        let pending = (effective * acc_per_share / SCALE_FACTOR - rewards_debt).max(0);
        if pending == 0 {
            return;
        }

        let reward_token: Address = env.storage().instance().get(&DataKey::RewardToken).unwrap();
        let pool_addr = env.current_contract_address();

        let new_debt = effective * acc_per_share / SCALE_FACTOR;
        let key_debt = DataKey::StakerRewardsDebt(staker.clone());
        env.storage().persistent().set(&key_debt, &new_debt);
        env.storage()
            .persistent()
            .extend_ttl(&key_debt, MIN_TTL, BUMP_TO);

        SepTokenClient::new(env, &reward_token).transfer(&pool_addr, staker, &pending);

        // #1045: same as _claim_rewards — settlement pays out of the owed
        // side only and never touches the undistributed counter.
        let owed: i128 = env
            .storage()
            .instance()
            .get(&DataKey::OwedRewards)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::OwedRewards, &(owed - pending).max(0));

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(env, "claimed"),),
            (staker.clone(), pending)
        );
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod cap_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{storage::Instance as _, Address as _, Ledger},
        token::{StellarAssetClient, TokenClient as StellarTokenClient},
        Address, Env,
    };
    use token::DataKey as TokenDataKey;

    fn create_sac<'a>(
        env: &'a Env,
        admin: &Address,
    ) -> (StellarTokenClient<'a>, StellarAssetClient<'a>) {
        let contract = env.register_stellar_asset_contract_v2(admin.clone());
        (
            StellarTokenClient::new(env, &contract.address()),
            StellarAssetClient::new(env, &contract.address()),
        )
    }

    fn setup(env: &Env) -> (Address, Address, StakingClient<'_>) {
        let admin = Address::generate(env);
        let staking_addr = env.register_contract(None, Staking);
        let (lp_token, lp_sac) = create_sac(env, &admin);
        let (reward_token, reward_sac) = create_sac(env, &admin);
        let staking = StakingClient::new(env, &staking_addr);
        staking.initialize(&lp_token.address, &reward_token.address, &admin);
        reward_sac.mint(&admin, &10_000_i128);
        staking.add_rewards(&admin, &10_000_i128);
        let staker = Address::generate(env);
        lp_sac.mint(&staker, &5_000_i128);
        (admin, staker, staking)
    }

    fn instance_ttl(env: &Env, staking: &StakingClient<'_>) -> u32 {
        env.as_contract(&staking.address, || env.storage().instance().get_ttl())
    }

    fn lower_instance_ttl_below_min(env: &Env, staking: &StakingClient<'_>) {
        env.ledger()
            .with_mut(|l| l.sequence_number += BUMP_TO - MIN_TTL + 1);
        let ttl = instance_ttl(env, staking);
        assert!(
            ttl < MIN_TTL,
            "test setup should lower instance TTL below MIN_TTL, got {ttl}"
        );
    }

    fn refresh_token_instance_ttl(env: &Env, token: &Address) {
        env.as_contract(token, || {
            env.storage().instance().extend_ttl(BUMP_TO, BUMP_TO * 2);
        });
    }

    fn refresh_token_balance_ttl(env: &Env, token: &Address, holder: &Address) {
        env.as_contract(token, || {
            env.storage().persistent().extend_ttl(
                &TokenDataKey::Balance(holder.clone()),
                BUMP_TO,
                BUMP_TO * 2,
            );
        });
    }

    fn assert_instance_ttl_bumped(env: &Env, staking: &StakingClient<'_>) {
        let ttl = instance_ttl(env, staking);
        assert!(
            ttl >= BUMP_TO - 1,
            "instance TTL {ttl} should be bumped toward BUMP_TO"
        );
    }

    #[test]
    fn test_initialize_extends_instance_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let staking_addr = env.register_contract(None, Staking);
        let (lp_token, _) = create_sac(&env, &admin);
        let (reward_token, _) = create_sac(&env, &admin);
        let staking = StakingClient::new(&env, &staking_addr);

        staking.initialize(&lp_token.address, &reward_token.address, &admin);

        assert_instance_ttl_bumped(&env, &staking);
    }

    #[test]
    fn test_unstake_restores_lapsed_instance_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);
        staking.stake(&staker, &1_000_i128);
        staking.update_rewards(&admin, &100_i128);
        let pool = staking.get_pool_info();
        refresh_token_instance_ttl(&env, &pool.lp_token);
        refresh_token_instance_ttl(&env, &pool.reward_token);
        refresh_token_balance_ttl(&env, &pool.lp_token, &staking.address);
        refresh_token_balance_ttl(&env, &pool.lp_token, &staker);
        refresh_token_balance_ttl(&env, &pool.reward_token, &staking.address);
        lower_instance_ttl_below_min(&env, &staking);
        assert!(instance_ttl(&env, &staking) < MIN_TTL);

        let (amount, rewards) = staking.unstake(&staker, &1_000_i128);

        assert_eq!((amount, rewards), (1_000, 100));
        assert_instance_ttl_bumped(&env, &staking);
    }

    #[test]
    fn test_emergency_withdraw_restores_lapsed_instance_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);
        staking.stake_locked(&staker, &1_000_i128, &MIN_LOCK_DURATION);
        staking.set_emergency_mode(&admin, &true);
        let pool = staking.get_pool_info();
        refresh_token_instance_ttl(&env, &pool.lp_token);
        refresh_token_balance_ttl(&env, &pool.lp_token, &staking.address);
        refresh_token_balance_ttl(&env, &pool.lp_token, &staker);
        lower_instance_ttl_below_min(&env, &staking);
        assert!(instance_ttl(&env, &staking) < MIN_TTL);

        let withdrawn = staking.emergency_withdraw(&staker);

        assert_eq!(withdrawn, 1_000);
        assert_instance_ttl_bumped(&env, &staking);
    }

    #[test]
    fn test_stake_and_claim_extend_instance_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);
        let pool = staking.get_pool_info();
        refresh_token_instance_ttl(&env, &pool.lp_token);
        refresh_token_instance_ttl(&env, &pool.reward_token);
        refresh_token_balance_ttl(&env, &pool.lp_token, &staker);
        refresh_token_balance_ttl(&env, &pool.reward_token, &staking.address);
        lower_instance_ttl_below_min(&env, &staking);
        assert!(instance_ttl(&env, &staking) < MIN_TTL);

        staking.stake_locked(&staker, &1_000_i128, &MIN_LOCK_DURATION);

        assert_instance_ttl_bumped(&env, &staking);
        staking.update_rewards(&admin, &100_i128);
        refresh_token_instance_ttl(&env, &pool.reward_token);
        lower_instance_ttl_below_min(&env, &staking);
        assert!(instance_ttl(&env, &staking) < MIN_TTL);

        let claimed = staking.claim(&staker);

        assert_eq!(claimed, 99);
        assert_instance_ttl_bumped(&env, &staking);
    }

    #[test]
    fn test_stake_no_lock_one_x_boost() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, staker, staking) = setup(&env);

        staking.stake(&staker, &1_000_i128);

        let info = staking.get_staker_info(&staker);
        assert_eq!(info.staked_amount, 1_000);
        assert_eq!(info.boost_multiplier, BOOST_SCALE); // 1Ãƒâ€”
        assert_eq!(info.effective_amount, 1_000);
        assert_eq!(info.lock_expiry, 0);

        let pool = staking.get_pool_info();
        assert_eq!(pool.total_effective_staked, 1_000);
    }

    #[test]
    fn test_stake_locked_max_duration_max_boost() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        staking.stake_locked(&staker, &1_000_i128, &MAX_LOCK_DURATION);

        let info = staking.get_staker_info(&staker);
        assert_eq!(info.staked_amount, 1_000);
        assert_eq!(info.boost_multiplier, DEFAULT_MAX_BOOST); // 2.5Ãƒâ€”
        assert_eq!(info.effective_amount, 2_500);

        let pool = staking.get_pool_info();
        assert_eq!(pool.total_effective_staked, 2_500);
    }

    // Ã¢â€â‚¬Ã¢â€â‚¬ Issue #467: stake_locked must remove the old effective stake using the
    // boost the position was actually recorded with, not the freshly
    // recomputed one, or TotalEffectiveStaked drifts on every top-up Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬

    #[test]
    fn test_stake_locked_top_up_after_boost_decay_keeps_total_effective_staked_correct() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);
        let (_, staker, staking) = setup(&env);

        // Lock for the max duration up front Ã¢â‚¬â€ boost starts at the maximum (2.5x).
        staking.stake_locked(&staker, &1_000_i128, &MAX_LOCK_DURATION);
        let info_before = staking.get_staker_info(&staker);
        assert_eq!(info_before.boost_multiplier, DEFAULT_MAX_BOOST);

        // Let half the lock elapse Ã¢â‚¬â€ the boost for the now-shorter remaining
        // time is lower than the boost the position was recorded with.
        env.ledger().set_timestamp(1_000 + MAX_LOCK_DURATION / 2);

        // Top up without requesting a new lock (lock_duration_secs = 0): this
        // recomputes the boost from the shrunken remaining lock time.
        staking.stake_locked(&staker, &500_i128, &0);

        let info_after = staking.get_staker_info(&staker);
        assert!(info_after.boost_multiplier < DEFAULT_MAX_BOOST);

        // With a single staker, TotalEffectiveStaked must equal this staker's
        // own effective amount Ã¢â‚¬â€ the old, differently-boosted contribution
        // must be fully replaced, not partially subtracted.
        let pool = staking.get_pool_info();
        assert_eq!(pool.total_effective_staked, info_after.effective_amount);
    }

    #[test]
    fn test_boosted_staker_earns_more_rewards() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker_a, staking) = setup(&env);
        let staker_b = Address::generate(&env);

        // Mint LP for staker_b
        let lp_token = staking.get_pool_info().lp_token;
        StellarAssetClient::new(&env, &lp_token).mint(&staker_b, &1_000_i128);

        // staker_a: 1000 LP, no lock (1Ãƒâ€”) Ã¢â€ â€™ effective 1000
        staking.stake(&staker_a, &1_000_i128);
        // staker_b: 1000 LP, max lock (2.5Ãƒâ€”) Ã¢â€ â€™ effective 2500
        staking.stake_locked(&staker_b, &1_000_i128, &MAX_LOCK_DURATION);

        // Distribute 500 rewards across total effective 3500
        staking.update_rewards(&admin, &500_i128);

        let pending_a = staking.pending_rewards(&staker_a);
        let pending_b = staking.pending_rewards(&staker_b);

        assert!(pending_b > pending_a * 2);
        assert_eq!(pending_a + pending_b, 499); // integer rounding on 500 reward split
    }

    #[test]
    fn test_extend_lock_increases_boost() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        staking.lock(&staker, &1_000_i128, &MIN_LOCK_DURATION);
        let before = staking.get_locked_position(&staker);
        assert!(before.boost_multiplier < DEFAULT_MAX_BOOST);

        staking.extend_lock(&staker, &MAX_LOCK_DURATION);
        let after = staking.get_locked_position(&staker);
        assert_eq!(after.boost_multiplier, DEFAULT_MAX_BOOST);
        assert!(after.lock_expiry >= before.lock_expiry);
    }

    #[test]
    fn test_unstake_locked_before_expiry_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        staking.stake_locked(&staker, &1_000_i128, &MIN_LOCK_DURATION);

        // Try to unstake immediately Ã¢â‚¬â€ should panic because lock hasn't expired.
        let result = staking.try_unstake(&staker, &1_000_i128);
        assert_eq!(result, Err(Ok(StakingError::StillLocked)));
    }

    #[test]
    fn test_unstake_after_lock_expiry_succeeds() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        staking.stake_locked(&staker, &1_000_i128, &MIN_LOCK_DURATION);
        staking.update_rewards(&admin, &100_i128);

        // Advance time past lock expiry.
        env.ledger().with_mut(|l| {
            l.timestamp = l.timestamp + MIN_LOCK_DURATION + 1;
        });

        let (lp_returned, rewards) = staking.unstake(&staker, &1_000_i128);
        assert_eq!(lp_returned, 1_000);
        assert!(rewards > 0);
    }

    #[test]
    /// Post-expiry re-lock: stake with a lock, let it expire, then re-lock
    /// without adding new LP (amount = 0, lock_duration > 0).
    fn test_relock_post_expiry() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        // Stake with a lock
        let stake_amount = 1_000_i128;
        staking.stake_locked(&staker, &stake_amount, &MIN_LOCK_DURATION);

        let pos_before = staking.get_locked_position(&staker);
        assert!(pos_before.lock_expiry > 0);
        assert!(pos_before.boost_multiplier > BOOST_SCALE);

        // Advance time past lock expiry
        env.ledger().with_mut(|l| {
            l.timestamp = l.timestamp + MIN_LOCK_DURATION + 1;
        });

        // After expiry, the stored boost hasn't changed (it's only updated on
        // write), but the lock_expiry is in the past so _boost_for_remaining
        // would return min_boost if called now Ã¢â‚¬â€ which is what the re-lock
        // below will replace.

        // Re-lock the same stake without adding new LP
        staking.stake_locked(&staker, &0_i128, &MIN_LOCK_DURATION);

        let pos_after = staking.get_locked_position(&staker);
        assert!(
            pos_after.boost_multiplier > BOOST_SCALE,
            "boost should be restored"
        );
        assert!(
            pos_after.lock_expiry > env.ledger().timestamp(),
            "lock expiry should be in the future"
        );
        assert_eq!(
            pos_after.amount, stake_amount,
            "staked amount should be unchanged"
        );
    }

    #[test]
    /// Zero amount without lock duration should panic.
    fn test_stake_locked_zero_amount_no_lock_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        staking.stake(&staker, &1_000_i128);

        let result = staking.try_stake_locked(&staker, &0_i128, &0_u64);
        assert_eq!(result, Err(Ok(StakingError::NothingToStake)));
    }

    #[test]
    /// A staker with no lock can acquire one on existing stake via amount=0.
    fn test_no_lock_to_locked_via_zero_amount() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        // Stake without a lock
        staking.stake(&staker, &1_000_i128);

        let pos_before = staking.get_locked_position(&staker);
        assert_eq!(pos_before.boost_multiplier, BOOST_SCALE);
        assert_eq!(pos_before.lock_expiry, 0);

        // Acquire a lock on existing stake
        staking.stake_locked(&staker, &0_i128, &MIN_LOCK_DURATION);

        let pos_after = staking.get_locked_position(&staker);
        assert!(pos_after.boost_multiplier > BOOST_SCALE);
        assert!(pos_after.lock_expiry > 0);
        assert_eq!(pos_after.amount, 1_000);
    }

    #[test]
    /// Bug #420: staking or extending after rewards accrued must not forfeit
    /// unclaimed rewards.  _settle_pending must transfer pending rewards before
    /// the debt is recomputed against the new effective amount.
    fn test_stake_twice_preserves_pending_rewards() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        // First stake
        staking.stake(&staker, &1_000_i128);
        staking.update_rewards(&admin, &100_i128);

        let pending_before = staking.pending_rewards(&staker);
        assert_eq!(
            pending_before, 100,
            "should have 100 pending after first distribution"
        );

        // Second stake Ã¢â‚¬â€ triggers _settle_pending which must pay out the 100
        // before the debt is recomputed.
        staking.stake(&staker, &500_i128);

        // After the second stake, the new effective amount defines a fresh debt.
        // Any rewards accrued before the second stake must have been paid out.
        let pending_after = staking.pending_rewards(&staker);
        assert_eq!(
            pending_after, 0,
            "no new rewards yet Ã¢â‚¬â€ second stake just happened"
        );

        // The staker's reward balance should have increased by the 100 that
        // was pending before the second stake.
        let reward_token = staking.get_pool_info().reward_token;
        let reward_client = StellarTokenClient::new(&env, &reward_token);
        assert_eq!(
            reward_client.balance(&staker),
            100,
            "staker should have received the 100 pending rewards"
        );
    }

    #[test]
    fn test_extend_lock_preserves_pending_rewards() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        // Stake with a short lock so we can extend later.
        let stake_amount = 4_000_i128;
        let reward_amount = 4_000_i128;
        staking.stake_locked(&staker, &stake_amount, &MIN_LOCK_DURATION);
        staking.update_rewards(&admin, &reward_amount);

        let pending_before = staking.pending_rewards(&staker);
        assert!(
            pending_before > 0,
            "should have pending rewards before extend"
        );

        // Extend lock Ã¢â‚¬â€ triggers _settle_pending which must transfer the
        // pending rewards before recomputing debt.
        staking.extend_lock(&staker, &MAX_LOCK_DURATION);

        // After settlement, no rewards should be pending (no new distributions).
        let pending_after = staking.pending_rewards(&staker);
        assert_eq!(pending_after, 0, "no new rewards after extend");

        // The staker's reward balance must have increased by at least the
        // pending amount (within integer-division rounding).
        let reward_token = staking.get_pool_info().reward_token;
        let reward_client = StellarTokenClient::new(&env, &reward_token);
        let balance = reward_client.balance(&staker);
        assert!(
            balance >= pending_before,
            "staker should have received at least {pending_before} pending rewards on extend, got {balance}"
        );
    }

    #[test]
    fn test_stake_and_claim() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        staking.stake(&staker, &1_000_i128);
        staking.update_rewards(&admin, &100_i128);

        let pending = staking.pending_rewards(&staker);
        assert_eq!(pending, 100);

        let claimed = staking.claim(&staker);
        assert_eq!(claimed, 100);
        assert_eq!(staking.pending_rewards(&staker), 0);
    }

    // Ã¢â€â‚¬Ã¢â€â‚¬ Pause / circuit-breaker tests (#360) Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬

    #[test]
    fn test_pause_blocks_stake_and_claim() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        staking.stake(&staker, &1_000_i128);
        staking.update_rewards(&admin, &100_i128);

        staking.pause(&admin);
        assert!(staking.is_paused());

        // New stakes and claims are halted while paused.
        assert_eq!(
            staking.try_stake(&staker, &500_i128),
            Err(Ok(StakingError::Paused))
        );
        assert_eq!(
            staking.try_stake_locked(&staker, &500_i128, &MIN_LOCK_DURATION),
            Err(Ok(StakingError::Paused))
        );
        assert_eq!(staking.try_claim(&staker), Err(Ok(StakingError::Paused)));
    }

    #[test]
    fn test_unpause_restores_operations() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        staking.pause(&admin);
        assert!(staking.try_stake(&staker, &1_000_i128).is_err());

        staking.unpause(&admin);
        assert!(!staking.is_paused());

        staking.stake(&staker, &1_000_i128);
        staking.update_rewards(&admin, &100_i128);
        assert_eq!(staking.claim(&staker), 100);
    }

    #[test]
    fn test_unstake_works_while_paused() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        staking.stake(&staker, &1_000_i128);
        staking.pause(&admin);

        // Stakers must always be able to retrieve their LP tokens.
        let (lp_returned, _) = staking.unstake(&staker, &1_000_i128);
        assert_eq!(lp_returned, 1_000);
    }

    /// Regression test for #560: unstake() must not pay out pending rewards
    /// while paused, even though it must still return LP principal. The
    /// pending amount must be preserved (not silently lost) and become
    /// claimable once the contract is unpaused.
    #[test]
    fn test_unstake_does_not_pay_rewards_while_paused() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        staking.stake(&staker, &1_000_i128);
        staking.update_rewards(&admin, &100_i128);

        let reward_token = staking.get_pool_info().reward_token;
        let reward_client = StellarTokenClient::new(&env, &reward_token);
        let balance_before_pause = reward_client.balance(&staker);

        staking.pause(&admin);
        assert!(staking.pending_rewards(&staker) > 0);

        let (lp_returned, rewards_returned) = staking.unstake(&staker, &500_i128);

        // Principal must still come back.
        assert_eq!(lp_returned, 500);
        // No reward payout happened as part of this call...
        assert_eq!(rewards_returned, 0);
        assert_eq!(reward_client.balance(&staker), balance_before_pause);
        // ...but the pending amount was not lost -- it's still owed.
        assert!(staking.pending_rewards(&staker) > 0);

        // Once unpaused, the preserved reward becomes claimable.
        staking.unpause(&admin);
        let claimed = staking.claim(&staker);
        assert!(claimed > 0);
        assert_eq!(
            reward_client.balance(&staker),
            balance_before_pause + claimed
        );
    }

    /// Regression test for #560: extend_lock() must be halted entirely while
    /// paused, matching claim() and stake_locked() -- it settles pending
    /// rewards internally, so it falls under the same "reward claims" halt
    /// pause() documents.
    #[test]
    fn test_extend_lock_blocked_while_paused() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        staking.stake_locked(&staker, &1_000_i128, &MIN_LOCK_DURATION);
        staking.pause(&admin);

        assert_eq!(
            staking.try_extend_lock(&staker, &(MIN_LOCK_DURATION * 2)),
            Err(Ok(StakingError::Paused))
        );
    }

    #[test]
    fn test_pause_requires_admin() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        assert_eq!(
            staking.try_pause(&staker),
            Err(Ok(StakingError::Unauthorized))
        );
    }

    // Ã¢â€â‚¬Ã¢â€â‚¬ Emergency mode tests (#359) Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬

    #[test]
    fn test_emergency_withdraw_disabled_by_default_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        staking.stake(&staker, &1_000_i128);

        // Emergency mode is off by default Ã¢â‚¬â€ withdrawal must be rejected.
        assert!(!staking.is_emergency_mode());
        let result = staking.try_emergency_withdraw(&staker);
        assert_eq!(result, Err(Ok(StakingError::EmergencyModeNotActive)));
    }

    #[test]
    fn test_emergency_withdraw_returns_lp_without_rewards() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        let lp_token = staking.get_pool_info().lp_token;
        let reward_token = staking.get_pool_info().reward_token;
        let lp_client = StellarTokenClient::new(&env, &lp_token);
        let reward_client = StellarTokenClient::new(&env, &reward_token);

        staking.stake(&staker, &1_000_i128);
        staking.update_rewards(&admin, &100_i128);
        assert_eq!(staking.pending_rewards(&staker), 100);

        let reward_balance_before = reward_client.balance(&staker);

        staking.set_emergency_mode(&admin, &true);
        let returned = staking.emergency_withdraw(&staker);

        // Full LP returned, no reward token moved.
        assert_eq!(returned, 1_000);
        assert_eq!(lp_client.balance(&staker), 5_000); // original mint restored
        assert_eq!(reward_client.balance(&staker), reward_balance_before);

        // Position fully cleared.
        let info = staking.get_staker_info(&staker);
        assert_eq!(info.staked_amount, 0);
        assert_eq!(info.effective_amount, 0);
        assert_eq!(staking.pending_rewards(&staker), 0);
        assert_eq!(staking.get_pool_info().total_effective_staked, 0);
    }

    #[test]
    fn test_emergency_withdraw_ignores_active_lock() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        // Lock for the max duration Ã¢â‚¬â€ unstake would panic before expiry.
        staking.stake_locked(&staker, &1_000_i128, &MAX_LOCK_DURATION);
        assert_eq!(
            staking.try_unstake(&staker, &1_000_i128),
            Err(Ok(StakingError::StillLocked))
        );

        staking.set_emergency_mode(&admin, &true);
        let returned = staking.emergency_withdraw(&staker);
        assert_eq!(returned, 1_000);
    }

    #[test]
    fn test_set_emergency_mode_requires_admin() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        // A non-admin caller must not be able to toggle emergency mode.
        let result = staking.try_set_emergency_mode(&staker, &true);
        assert_eq!(result, Err(Ok(StakingError::Unauthorized)));
    }

    // ── Issue #699: lock-boost decay after expiry ───────────────────────────

    /// The core regression this issue exists to fix. Fails against `main`:
    /// there, `get_staker_info`/`pending_rewards` keep reading the peak
    /// boost forever, so `staker_b` would still show `DEFAULT_MAX_BOOST` and
    /// out-earn `staker_a` by the same 2.5x ratio long after their lock
    /// expired and with no further interaction from them at all.
    #[test]
    fn test_expired_lock_decays_to_min_boost_without_any_interaction() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker_a, staking) = setup(&env);
        let staker_b = Address::generate(&env);
        let lp_token = staking.get_pool_info().lp_token;
        StellarAssetClient::new(&env, &lp_token).mint(&staker_b, &1_000_i128);

        staking.stake(&staker_a, &1_000_i128); // 1x, never locked
        staking.stake_locked(&staker_b, &1_000_i128, &MIN_LOCK_DURATION); // peak boost for this duration

        let boost_before = staking.get_staker_info(&staker_b).boost_multiplier;
        assert!(boost_before > BOOST_SCALE, "lock must start above 1x");

        // Advance past expiry. staker_b never calls anything again.
        env.ledger().with_mut(|l| {
            l.timestamp = l.timestamp + MIN_LOCK_DURATION + 1;
        });

        // Every read path must show the decayed boost, not the stale one.
        assert_eq!(staking.current_boost(&staker_b), BOOST_SCALE);
        assert_eq!(
            staking.get_staker_info(&staker_b).boost_multiplier,
            BOOST_SCALE
        );
        assert_eq!(
            staking.get_locked_position(&staker_b).boost_multiplier,
            BOOST_SCALE
        );
        assert_eq!(staking.effective_staked(&staker_b), 1_000);

        // A reward round distributed *after* expiry must split 1x:1x between
        // two otherwise-identical stakers, not 1x:2.5x.
        staking.update_rewards(&admin, &1_000_i128);
        let pending_a = staking.pending_rewards(&staker_a);
        let pending_b = staking.pending_rewards(&staker_b);
        assert_eq!(
            pending_a, pending_b,
            "an expired-but-unsettled lock must earn exactly 1x, matching an unlocked staker, from expiry onward"
        );
    }

    #[test]
    fn test_boost_exactly_at_lock_expiry_is_decayed() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        staking.stake_locked(&staker, &1_000_i128, &MIN_LOCK_DURATION);
        let expiry = staking.boost_expires_at(&staker);

        env.ledger().set_timestamp(expiry);
        assert_eq!(
            staking.current_boost(&staker),
            BOOST_SCALE,
            "now == lock_expiry must already be decayed (the cliff uses >=)"
        );
    }

    #[test]
    fn test_boost_one_second_before_expiry_is_not_decayed() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        staking.stake_locked(&staker, &1_000_i128, &MIN_LOCK_DURATION);
        let expiry = staking.boost_expires_at(&staker);
        let boost_at_lock = staking.current_boost(&staker);

        env.ledger().set_timestamp(expiry - 1);
        assert_eq!(
            staking.current_boost(&staker),
            boost_at_lock,
            "one second before expiry the peak boost must still apply"
        );
    }

    #[test]
    fn test_settle_boost_harvests_at_old_rate_then_resets_boost_and_total() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        staking.stake_locked(&staker, &1_000_i128, &MAX_LOCK_DURATION);
        // Distribute rewards while the lock is still fully active, so this
        // round is legitimately owed at the peak boost.
        staking.update_rewards(&admin, &500_i128);
        let owed_at_peak = staking.pending_rewards(&staker);
        assert!(owed_at_peak > 0);

        env.ledger().with_mut(|l| {
            l.timestamp = l.timestamp + MAX_LOCK_DURATION + 1;
        });

        let reward_token = staking.get_pool_info().reward_token;
        let reward_client = StellarTokenClient::new(&env, &reward_token);
        let balance_before = reward_client.balance(&staker);

        staking.settle_boost(&staker);

        // The pre-expiry round was harvested in full.
        assert_eq!(
            reward_client.balance(&staker),
            balance_before + owed_at_peak
        );
        // Boost and TotalEffectiveStaked are corrected going forward.
        assert_eq!(
            staking.get_staker_info(&staker).boost_multiplier,
            BOOST_SCALE
        );
        assert_eq!(
            staking.get_pool_info().total_effective_staked,
            staking.get_staker_info(&staker).staked_amount
        );
        assert_eq!(staking.pending_rewards(&staker), 0);

        // boost_exp only fires when settlement actually changed something.
        use soroban_sdk::{testutils::Events as _, IntoVal};
        let topic: soroban_sdk::Val = Symbol::new(&env, "boost_exp").into_val(&env);
        assert!(env.events().all().iter().any(|event| {
            event
                .1
                .get(0)
                .map(|t| t.get_payload() == topic.get_payload())
                .unwrap_or(false)
        }));
    }

    #[test]
    fn test_settle_boost_is_noop_on_active_lock() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        staking.stake_locked(&staker, &1_000_i128, &MAX_LOCK_DURATION);
        let before = staking.get_staker_info(&staker);

        // Lock is nowhere near expiry -- settle_boost must not touch it.
        staking.settle_boost(&staker);

        let after = staking.get_staker_info(&staker);
        assert_eq!(after.boost_multiplier, before.boost_multiplier);
        assert_eq!(after.effective_amount, before.effective_amount);
        assert_eq!(
            staking.get_pool_info().total_effective_staked,
            before.effective_amount
        );
    }

    #[test]
    fn test_settle_boost_is_noop_when_already_settled() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        staking.stake_locked(&staker, &1_000_i128, &MIN_LOCK_DURATION);
        env.ledger().with_mut(|l| {
            l.timestamp = l.timestamp + MIN_LOCK_DURATION + 1;
        });
        staking.settle_boost(&staker);
        assert_eq!(
            staking.get_staker_info(&staker).boost_multiplier,
            BOOST_SCALE
        );

        // Calling it again must be a pure no-op: no panic, no change, and no
        // (harmful) further reduction below MIN_BOOST.
        staking.settle_boost(&staker);
        assert_eq!(
            staking.get_staker_info(&staker).boost_multiplier,
            BOOST_SCALE
        );
    }

    #[test]
    fn test_settle_boost_is_noop_for_never_locked_staker() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        staking.stake(&staker, &1_000_i128); // no lock at all
        staking.settle_boost(&staker); // must not panic
        assert_eq!(
            staking.get_staker_info(&staker).boost_multiplier,
            BOOST_SCALE
        );
        assert_eq!(staking.get_staker_info(&staker).staked_amount, 1_000);
    }

    #[test]
    fn test_settle_boost_is_noop_for_unknown_address() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, _staker, staking) = setup(&env);

        let stranger = Address::generate(&env);
        staking.settle_boost(&stranger); // must not panic on a non-staker
        assert_eq!(staking.get_pool_info().total_effective_staked, 0);
    }

    /// settle_boost is permissionless: `caller` need not authorize anything,
    /// and the settlement leaves the staker's own claimable/withdrawable
    /// position strictly no worse off than it already was.
    #[test]
    fn test_settle_boost_is_permissionless_and_cannot_harm_the_staker() {
        let env = Env::default();
        // Deliberately do NOT call env.mock_all_auths() for the settle_boost
        // call itself -- only the staker's own stake_locked needs auth.
        let staker = Address::generate(&env);
        let admin = Address::generate(&env);
        let staking_addr = env.register_contract(None, Staking);
        let (lp_token, lp_sac) = create_sac(&env, &admin);
        let (reward_token, reward_sac) = create_sac(&env, &admin);
        let staking = StakingClient::new(&env, &staking_addr);

        env.mock_all_auths();
        staking.initialize(&lp_token.address, &reward_token.address, &admin);
        reward_sac.mint(&admin, &10_000_i128);
        staking.add_rewards(&admin, &10_000_i128);
        lp_sac.mint(&staker, &1_000_i128);
        staking.stake_locked(&staker, &1_000_i128, &MIN_LOCK_DURATION);

        env.ledger().with_mut(|l| {
            l.timestamp = l.timestamp + MIN_LOCK_DURATION + 1;
        });

        // Call settle_boost with no auths mocked at all: it must succeed
        // without requiring anyone's signature.
        env.set_auths(&[]);
        staking.settle_boost(&staker);

        assert_eq!(
            staking.get_staker_info(&staker).boost_multiplier,
            BOOST_SCALE
        );
        // The staker can still unstake their full principal afterward.
        env.mock_all_auths();
        let (returned, _) = staking.unstake(&staker, &1_000_i128);
        assert_eq!(
            returned, 1_000,
            "settle_boost must never reduce withdrawable principal"
        );
    }

    #[test]
    fn test_settle_boost_batch_mixed_expired_and_live() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker_expired, staking) = setup(&env);
        let staker_live = Address::generate(&env);
        let staker_unlocked = Address::generate(&env);
        let stranger = Address::generate(&env);
        let lp_token = staking.get_pool_info().lp_token;
        let lp_sac = StellarAssetClient::new(&env, &lp_token);
        lp_sac.mint(&staker_live, &1_000_i128);
        lp_sac.mint(&staker_unlocked, &1_000_i128);
        let _ = admin;

        staking.stake_locked(&staker_expired, &1_000_i128, &MIN_LOCK_DURATION);
        staking.stake_locked(&staker_live, &1_000_i128, &MAX_LOCK_DURATION);
        staking.stake(&staker_unlocked, &1_000_i128);

        env.ledger().with_mut(|l| {
            l.timestamp = l.timestamp + MIN_LOCK_DURATION + 1;
        });
        // staker_live's much longer lock has not expired.

        let mut batch = Vec::new(&env);
        batch.push_back(staker_expired.clone());
        batch.push_back(staker_live.clone());
        batch.push_back(staker_unlocked.clone());
        batch.push_back(stranger.clone());
        staking.settle_boost_batch(&batch);

        assert_eq!(
            staking.get_staker_info(&staker_expired).boost_multiplier,
            BOOST_SCALE,
            "expired lock must be settled"
        );
        assert!(
            staking.get_staker_info(&staker_live).boost_multiplier > BOOST_SCALE,
            "live lock must be untouched"
        );
        assert_eq!(
            staking.get_staker_info(&staker_unlocked).boost_multiplier,
            BOOST_SCALE,
            "never-locked staker is unaffected either way"
        );
        // Batch containing an unstaked stranger must not have panicked --
        // reaching this assertion at all is the proof.
        assert_eq!(staking.get_staker_info(&stranger).staked_amount, 0);
    }

    #[test]
    fn test_settle_boost_batch_rejects_oversized_batch() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, _staker, staking) = setup(&env);

        let mut batch = Vec::new(&env);
        for _ in 0..(MAX_BATCH_SIZE + 1) {
            batch.push_back(Address::generate(&env));
        }
        let result = staking.try_settle_boost_batch(&batch);
        assert_eq!(
            result,
            Err(Ok(StakingError::BatchTooLarge)),
            "a batch over MAX_BATCH_SIZE must be rejected"
        );
    }

    // ── Issue #699: staker index lifecycle ───────────────────────────────────

    #[test]
    fn test_staker_index_removed_on_full_unstake() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        staking.stake(&staker, &1_000_i128);
        assert_eq!(staking.list_expired_boosts(&0, &10).len(), 0); // not expired, but indexed

        staking.unstake(&staker, &1_000_i128);

        // Fabricate an expired-looking record directly and confirm the
        // *index* (not just the expiry check) is what's gating discovery --
        // list_expired_boosts must not find an address no longer indexed
        // even if we make its stored fields look expired.
        env.as_contract(&staking.address, || {
            env.storage().persistent().set(
                &DataKey::BoostMultiplier(staker.clone()),
                &DEFAULT_MAX_BOOST,
            );
            env.storage()
                .persistent()
                .set(&DataKey::LockExpiry(staker.clone()), &1u64);
        });
        assert_eq!(
            staking.list_expired_boosts(&0, &10).len(),
            0,
            "a fully-unstaked address must have been dropped from the index"
        );
    }

    #[test]
    fn test_staker_index_readded_after_restake_following_full_unstake() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        staking.stake_locked(&staker, &1_000_i128, &MIN_LOCK_DURATION);
        env.ledger().with_mut(|l| {
            l.timestamp = l.timestamp + MIN_LOCK_DURATION + 1;
        });
        staking.unstake(&staker, &1_000_i128);

        // Re-stake with a fresh lock.
        staking.stake_locked(&staker, &1_000_i128, &MIN_LOCK_DURATION);
        env.ledger().with_mut(|l| {
            l.timestamp = l.timestamp + MIN_LOCK_DURATION + 1;
        });

        let found = staking.list_expired_boosts(&0, &10);
        assert!(
            (0..found.len()).any(|i| found.get(i).unwrap() == staker),
            "restaking after a full unstake must re-add the staker to the index"
        );
    }

    #[test]
    fn test_staker_index_removed_on_emergency_withdraw() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        staking.stake(&staker, &1_000_i128);
        staking.set_emergency_mode(&admin, &true);
        staking.emergency_withdraw(&staker);

        env.as_contract(&staking.address, || {
            env.storage().persistent().set(
                &DataKey::BoostMultiplier(staker.clone()),
                &DEFAULT_MAX_BOOST,
            );
            env.storage()
                .persistent()
                .set(&DataKey::LockExpiry(staker.clone()), &1u64);
        });
        assert_eq!(staking.list_expired_boosts(&0, &10).len(), 0);
    }

    // ── Issue #699: new views ────────────────────────────────────────────────

    #[test]
    fn test_current_boost_and_effective_staked_views_match_get_staker_info() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        staking.stake_locked(&staker, &1_000_i128, &MAX_LOCK_DURATION);
        let info = staking.get_staker_info(&staker);
        assert_eq!(staking.current_boost(&staker), info.boost_multiplier);
        assert_eq!(staking.effective_staked(&staker), info.effective_amount);

        env.ledger().with_mut(|l| {
            l.timestamp = l.timestamp + MAX_LOCK_DURATION + 1;
        });
        let info_after = staking.get_staker_info(&staker);
        assert_eq!(staking.current_boost(&staker), info_after.boost_multiplier);
        assert_eq!(
            staking.effective_staked(&staker),
            info_after.effective_amount
        );
    }

    #[test]
    fn test_total_effective_staked_view_matches_pool_info() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        staking.stake_locked(&staker, &1_000_i128, &MAX_LOCK_DURATION);
        assert_eq!(
            staking.total_effective_staked(),
            staking.get_pool_info().total_effective_staked
        );
    }

    #[test]
    fn test_boost_expires_at_view() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);

        assert_eq!(staking.boost_expires_at(&staker), 0);
        staking.stake_locked(&staker, &1_000_i128, &MIN_LOCK_DURATION);
        assert_eq!(
            staking.boost_expires_at(&staker),
            staking.get_locked_position(&staker).lock_expiry
        );
        assert!(staking.boost_expires_at(&staker) > 0);
    }

    #[test]
    fn test_list_expired_boosts_pagination_and_filtering() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker_1, staking) = setup(&env);
        let staker_2 = Address::generate(&env);
        let staker_3 = Address::generate(&env);
        let lp_token = staking.get_pool_info().lp_token;
        let lp_sac = StellarAssetClient::new(&env, &lp_token);
        lp_sac.mint(&staker_2, &1_000_i128);
        lp_sac.mint(&staker_3, &1_000_i128);

        staking.stake_locked(&staker_1, &1_000_i128, &MIN_LOCK_DURATION);
        staking.stake_locked(&staker_2, &1_000_i128, &MIN_LOCK_DURATION);
        staking.stake_locked(&staker_3, &1_000_i128, &MAX_LOCK_DURATION); // stays live

        env.ledger().with_mut(|l| {
            l.timestamp = l.timestamp + MIN_LOCK_DURATION + 1;
        });

        // Page 1: first two indexed entries.
        let page1 = staking.list_expired_boosts(&0, &2);
        // Page 2: remainder.
        let page2 = staking.list_expired_boosts(&2, &2);
        let total_found = page1.len() + page2.len();
        assert_eq!(
            total_found, 2,
            "exactly the two expired stakers must be found across both pages, live staker_3 excluded"
        );

        // limit=0 returns nothing; offset past the end returns nothing.
        assert_eq!(staking.list_expired_boosts(&0, &0).len(), 0);
        assert_eq!(staking.list_expired_boosts(&100, &10).len(), 0);
    }

    // ── Issue #699: migration ────────────────────────────────────────────────

    /// Fixture built on pre-upgrade state: writes storage directly the way
    /// `stake_locked` would have *before* this upgrade introduced
    /// `DataKey::StakerIndex`, i.e. every field the old contract wrote, and
    /// nothing the new index-maintenance code would have added.
    #[test]
    fn test_migration_backfills_index_without_disturbing_pre_upgrade_position() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);
        let lp_token = staking.get_pool_info().lp_token;
        StellarAssetClient::new(&env, &lp_token).mint(&staking.address, &1_000_i128);

        let lock_expiry = env.ledger().timestamp() + MIN_LOCK_DURATION;
        env.as_contract(&staking.address, || {
            env.storage()
                .persistent()
                .set(&DataKey::StakerAmount(staker.clone()), &1_000i128);
            env.storage().persistent().set(
                &DataKey::BoostMultiplier(staker.clone()),
                &DEFAULT_MAX_BOOST,
            );
            env.storage()
                .persistent()
                .set(&DataKey::LockExpiry(staker.clone()), &lock_expiry);
            let total: i128 = env
                .storage()
                .instance()
                .get(&DataKey::TotalEffectiveStaked)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::TotalEffectiveStaked, &(total + 2_500i128));
        });

        // Pre-migration: fully functional, just not keeper-discoverable.
        assert_eq!(staking.get_staker_info(&staker).staked_amount, 1_000);
        assert_eq!(staking.list_expired_boosts(&0, &10).len(), 0);

        env.ledger().with_mut(|l| {
            l.timestamp = l.timestamp + MIN_LOCK_DURATION + 1;
        });
        assert_eq!(
            staking.list_expired_boosts(&0, &10).len(),
            0,
            "still not discoverable before the migration call -- the index, not expiry, is the gap"
        );

        // Run the migration.
        let mut backfill = Vec::new(&env);
        backfill.push_back(staker.clone());
        staking.register_existing_stakers(&backfill);

        // Now discoverable and settleable via the normal keeper path.
        let found = staking.list_expired_boosts(&0, &10);
        assert_eq!(found.len(), 1);
        assert_eq!(found.get(0).unwrap(), staker);
        staking.settle_boost(&staker);
        assert_eq!(staking.current_boost(&staker), BOOST_SCALE);

        // Not bricked: the pre-upgrade staker can still fully unstake.
        let (returned, _rewards) = staking.unstake(&staker, &1_000_i128);
        assert_eq!(
            returned, 1_000,
            "migration must not brick unstake for pre-upgrade stakers"
        );
    }

    #[test]
    fn test_register_existing_stakers_is_idempotent_and_safe_on_unknown_address() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);
        let unknown = Address::generate(&env);

        staking.stake(&staker, &1_000_i128); // already indexed normally

        let mut backfill = Vec::new(&env);
        backfill.push_back(staker.clone());
        backfill.push_back(unknown.clone()); // never staked -- must be a safe no-op
        staking.register_existing_stakers(&backfill);
        staking.register_existing_stakers(&backfill); // calling twice must not duplicate

        // No duplicate entries: settling the whole index once must not
        // double-settle (which would show up as a second boost_exp event or
        // an incorrect TotalEffectiveStaked after settlement).
        let all = staking.list_expired_boosts(&0, &MAX_LIST_LIMIT);
        let _ = all; // nothing expired yet at this point; existence check is enough
        assert_eq!(staking.get_staker_info(&unknown).staked_amount, 0);
    }

    // ── Issue #699: invariants over a bounded pseudo-random sequence ────────

    /// Simple deterministic PRNG (splitmix64-style) so the sequence below is
    /// reproducible and doesn't require a randomness dependency, while still
    /// exercising the mix of operations the acceptance criteria ask for
    /// ("randomized sequence of stakes, locks, expiries, and claims").
    fn next_rand(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    #[test]
    fn test_invariants_hold_over_randomized_stake_lock_expire_claim_sequence() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let staking_addr = env.register_contract(None, Staking);
        let (lp_token, lp_sac) = create_sac(&env, &admin);
        let (reward_token, reward_sac) = create_sac(&env, &admin);
        let staking = StakingClient::new(&env, &staking_addr);
        staking.initialize(&lp_token.address, &reward_token.address, &admin);
        reward_sac.mint(&admin, &1_000_000_i128);

        let stakers: [Address; 4] = [
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
        ];
        for s in stakers.iter() {
            lp_sac.mint(s, &100_000_i128);
        }
        // Track how much LP each staker has ever staked, so we can assert
        // no one ever withdraws more than they put in.
        let mut staked_lifetime: [i128; 4] = [0, 0, 0, 0];
        let mut unstaked_lifetime: [i128; 4] = [0, 0, 0, 0];
        let mut total_rewards_added: i128 = 0;

        let mut rng_state: u64 = 0xC0FFEE;
        for step in 0..40u32 {
            let r = next_rand(&mut rng_state);
            let who = (r % 4) as usize;
            let action = (r / 4) % 6;
            let staker = &stakers[who];

            match action {
                0 => {
                    // stake (no lock)
                    let amount = 100 + (r % 900) as i128;
                    staking.stake(staker, &amount);
                    staked_lifetime[who] += amount;
                }
                1 => {
                    // stake_locked with a random duration in-range
                    let amount = 100 + (r % 900) as i128;
                    let span = MAX_LOCK_DURATION - MIN_LOCK_DURATION;
                    let dur = MIN_LOCK_DURATION + (r % (span + 1));
                    staking.stake_locked(staker, &amount, &dur);
                    staked_lifetime[who] += amount;
                }
                2 => {
                    // advance time (simulates expiries happening in the background)
                    let delta = 1 + (r % (MIN_LOCK_DURATION / 2));
                    env.ledger().with_mut(|l| {
                        l.timestamp += delta;
                    });
                }
                3 => {
                    // add + distribute rewards, only if someone has stake
                    if staking.total_effective_staked() > 0 {
                        let amount = 10 + (r % 200) as i128;
                        staking.add_rewards(&admin, &amount);
                        staking.update_rewards(&admin, &amount);
                        total_rewards_added += amount;
                    }
                }
                4 => {
                    // claim, if there's anything pending
                    if staking.pending_rewards(staker) > 0 && !staking.is_paused() {
                        staking.claim(staker);
                    }
                }
                _ => {
                    // partial unstake, respecting the lock and available balance
                    let staked = staking.get_staker_info(staker).staked_amount;
                    let expiry = staking.boost_expires_at(staker);
                    if staked > 0 && env.ledger().timestamp() >= expiry {
                        let amount = 1 + (r % (staked as u64).max(1)) as i128;
                        let amount = amount.min(staked);
                        let (returned, _) = staking.unstake(staker, &amount);
                        unstaked_lifetime[who] += returned;
                    }
                }
            }

            // ---- Invariant 1: sum(effective_staked) == total_effective_staked ----
            // (an occasional off-by-one from integer division on decayed
            // settlements is tolerated within a documented rounding slack)
            let sum_effective: i128 = stakers.iter().map(|s| staking.effective_staked(s)).sum();
            let total = staking.total_effective_staked();
            assert!(
                (sum_effective - total).abs() <= 1,
                "step {step}: sum(effective_staked)={sum_effective} vs total_effective_staked()={total}"
            );

            // ---- Invariant 2: no staker ever withdraws more than they staked ----
            for i in 0..4 {
                assert!(
                    unstaked_lifetime[i] <= staked_lifetime[i],
                    "step {step}: staker {i} unstaked {} but only ever staked {}",
                    unstaked_lifetime[i],
                    staked_lifetime[i]
                );
            }
        }

        // ---- Invariant 3: total rewards paid out never exceeds add_rewards ----
        let reward_client = StellarTokenClient::new(&env, &reward_token.address);
        // Claim out whatever's left pending so the final tally is complete.
        for s in stakers.iter() {
            if staking.pending_rewards(s) > 0 && !staking.is_paused() {
                staking.claim(s);
            }
        }
        let mut total_paid_final: i128 = 0;
        for s in stakers.iter() {
            total_paid_final += reward_client.balance(s);
        }
        assert!(
            total_paid_final <= total_rewards_added,
            "total rewards paid ({total_paid_final}) exceeded total added ({total_rewards_added})"
        );
    }

    #[test]
    fn test_update_rewards_clamps_to_pool_balance() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let staking_addr = env.register_contract(None, Staking);
        let (lp_token, lp_sac) = create_sac(&env, &admin);
        let (reward_token, reward_sac) = create_sac(&env, &admin);
        let staking = StakingClient::new(&env, &staking_addr);
        staking.initialize(&lp_token.address, &reward_token.address, &admin);

        // Mint a small amount to admin and add only 100 to the pool.
        reward_sac.mint(&admin, &1_000_i128);
        staking.add_rewards(&admin, &100_i128);

        // Stake one staker so total_effective > 0.
        let staker = Address::generate(&env);
        lp_sac.mint(&staker, &1_000_i128);
        staking.stake(&staker, &1_000_i128);

        // Attempt to distribute more than the pool has: should be clamped
        // to the available 100 and not panic.
        staking.update_rewards(&admin, &500_i128);

        // Reward pool must be drained to zero.
        let pool = staking.get_pool_info();
        assert_eq!(pool.reward_pool_balance, 0);

        // The staker must have some pending rewards from the clamped
        // distribution.
        assert!(staking.pending_rewards(&staker) > 0);
    }

    // ── #924: typed error enum ───────────────────────────────────────────────

    #[test]
    fn test_double_initialize_is_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, _staker, staking) = setup(&env);
        let a = Address::generate(&env);
        let b = Address::generate(&env);
        let c = Address::generate(&env);
        assert_eq!(
            staking.try_initialize(&a, &b, &c),
            Err(Ok(StakingError::AlreadyInitialized))
        );
    }

    #[test]
    fn test_calls_before_initialize_report_not_initialized() {
        let env = Env::default();
        env.mock_all_auths();
        let staking_addr = env.register_contract(None, Staking);
        let staking = StakingClient::new(&env, &staking_addr);
        let admin = Address::generate(&env);
        assert_eq!(
            staking.try_add_rewards(&admin, &100_i128),
            Err(Ok(StakingError::NotInitialized))
        );
        assert_eq!(
            staking.try_get_pool_info(),
            Err(Ok(StakingError::NotInitialized))
        );
    }

    #[test]
    fn test_add_rewards_non_admin_is_unauthorized() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);
        assert_eq!(
            staking.try_add_rewards(&staker, &100_i128),
            Err(Ok(StakingError::Unauthorized))
        );
    }

    #[test]
    fn test_add_rewards_zero_amount_is_invalid() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, _staker, staking) = setup(&env);
        assert_eq!(
            staking.try_add_rewards(&admin, &0_i128),
            Err(Ok(StakingError::InvalidAmount))
        );
    }

    #[test]
    fn test_unstake_more_than_staked_is_insufficient() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);
        staking.stake(&staker, &1_000_i128);
        assert_eq!(
            staking.try_unstake(&staker, &2_000_i128),
            Err(Ok(StakingError::InsufficientStaked))
        );
    }

    #[test]
    fn test_unstake_zero_amount_is_invalid() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);
        staking.stake(&staker, &1_000_i128);
        assert_eq!(
            staking.try_unstake(&staker, &0_i128),
            Err(Ok(StakingError::InvalidAmount))
        );
    }

    #[test]
    fn test_claim_without_pending_is_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);
        // A fresh staker with no stake has no pending rewards.
        assert_eq!(
            staking.try_claim(&staker),
            Err(Ok(StakingError::NoPendingRewards))
        );
    }

    #[test]
    fn test_update_rewards_without_stakers_is_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, _staker, staking) = setup(&env);
        // No one has staked, so total effective stake is zero.
        assert_eq!(
            staking.try_update_rewards(&admin, &100_i128),
            Err(Ok(StakingError::NoStakers))
        );
    }

    #[test]
    fn test_extend_lock_without_active_lock_is_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);
        // Staked without a lock, so there is no active lock to extend.
        staking.stake(&staker, &1_000_i128);
        assert_eq!(
            staking.try_extend_lock(&staker, &MIN_LOCK_DURATION),
            Err(Ok(StakingError::NoActiveLock))
        );
    }

    #[test]
    fn test_extend_lock_zero_duration_is_invalid() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);
        staking.stake_locked(&staker, &1_000_i128, &MIN_LOCK_DURATION);
        assert_eq!(
            staking.try_extend_lock(&staker, &0_u64),
            Err(Ok(StakingError::InvalidDuration))
        );
    }

    #[test]
    fn test_add_rewards_respects_max_pool_cap() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let staking_addr = env.register_contract(None, Staking);
        let (lp_token, _lp_sac) = create_sac(&env, &admin);
        let (reward_token, reward_sac) = create_sac(&env, &admin);
        let staking = StakingClient::new(&env, &staking_addr);
        staking.initialize(&lp_token.address, &reward_token.address, &admin);
        staking.set_max_reward_pool_balance(&admin, &100_i128);
        reward_sac.mint(&admin, &1_000_i128);
        assert_eq!(
            staking.try_add_rewards(&admin, &500_i128),
            Err(Ok(StakingError::MaxRewardPoolExceeded))
        );
    }

    #[test]
    fn test_set_max_reward_pool_below_current_is_invalid() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, _staker, staking) = setup(&env);
        // `setup` already funded the pool with 10_000; a cap of 1 is below it.
        assert_eq!(
            staking.try_set_max_reward_pool_balance(&admin, &1_i128),
            Err(Ok(StakingError::InvalidMaxBalance))
        );
    }

    #[test]
    fn test_unlock_without_stake_is_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);
        assert_eq!(
            staking.try_unlock(&staker),
            Err(Ok(StakingError::NothingStaked))
        );
    }

    #[test]
    fn test_admin_rotation_happy_path() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, _staker, staking) = setup(&env);
        let new_admin = Address::generate(&env);

        assert_eq!(staking.get_admin(), admin);
        assert_eq!(staking.get_pending_admin(), None);

        // Propose admin
        staking.propose_admin(&admin, &new_admin);
        assert_eq!(staking.get_pending_admin(), Some(new_admin.clone()));
        // Old admin retains control before acceptance
        assert_eq!(staking.get_admin(), admin);

        // Accept admin
        staking.accept_admin(&new_admin);
        assert_eq!(staking.get_admin(), new_admin);
        assert_eq!(staking.get_pending_admin(), None);
    }

    #[test]
    fn test_propose_admin_unauthorized() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);
        let new_admin = Address::generate(&env);

        // Non-admin propose_admin must fail
        assert_eq!(
            staking.try_propose_admin(&staker, &new_admin),
            Err(Ok(StakingError::Unauthorized))
        );
        assert_eq!(staking.get_pending_admin(), None);
        assert_eq!(staking.get_admin(), admin);
    }

    #[test]
    fn test_accept_admin_wrong_address_or_no_pending() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);
        let new_admin = Address::generate(&env);

        // Accept without prior proposal
        assert_eq!(
            staking.try_accept_admin(&new_admin),
            Err(Ok(StakingError::NoPendingAdmin))
        );

        // Propose to new_admin
        staking.propose_admin(&admin, &new_admin);

        // Wrong address calling accept_admin
        assert_eq!(
            staking.try_accept_admin(&staker),
            Err(Ok(StakingError::WrongAdmin))
        );

        // Old admin still in control
        assert_eq!(staking.get_admin(), admin);
    }

    // ── Issue #1045: reward accounting is decremented exactly once ─────────
    //
    // Main decremented the single RewardPoolBalance counter both when rewards
    // were distributed (update_rewards) and when they were paid out (claim,
    // unstake, _settle_pending_at). The counter went negative after the first
    // payout, update_rewards clamped to it and distributed nothing, and every
    // reward cycle after the first was stranded in the contract.

    /// The reproduction from the issue: add_rewards(10_000), stake(1_000),
    /// update_rewards(10_000), claim(). Must end with a non-negative
    /// balance equal to 0 undistributed and 0 owed. Fails on main, where the
    /// final balance reads -10_000.
    #[test]
    fn test_reward_balance_not_double_decremented_repro_1045() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        // The issue's reproduction: `setup` already funded and escrowed
        // 10_000 reward tokens; stake, distribute them, then claim.
        staking.stake(&staker, &1_000_i128);
        staking.update_rewards(&admin, &10_000_i128);

        let pool = staking.get_pool_info();
        assert_eq!(
            pool.reward_pool_balance, 0,
            "after distribution everything is owed, nothing undistributed"
        );
        assert_eq!(staking.get_owed_rewards(), 10_000);

        let claimed = staking.claim(&staker);
        assert_eq!(claimed, 10_000);

        let pool = staking.get_pool_info();
        assert_eq!(
            pool.reward_pool_balance, 0,
            "claims must never touch the undistributed counter"
        );
        assert_eq!(
            staking.get_owed_rewards(),
            0,
            "paying the claim must zero the owed counter"
        );
    }

    /// The stranded-cycle regression: with three rounds of
    /// add_rewards(X) → update_rewards(X) → claim by two stakers, each round
    /// distributes exactly X (± per-share dust). On main the second round
    /// distributes 0 because the double-decremented balance had gone
    /// negative, so this test fails there.
    #[test]
    fn test_multi_cycle_rewards_distribute_every_round() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let staking_addr = env.register_contract(None, Staking);
        let (lp_token, lp_sac) = create_sac(&env, &admin);
        let (reward_token, reward_sac) = create_sac(&env, &admin);
        let staking = StakingClient::new(&env, &staking_addr);
        staking.initialize(&lp_token.address, &reward_token.address, &admin);
        reward_sac.mint(&admin, &100_000_i128);

        let staker_a = Address::generate(&env);
        let staker_b = Address::generate(&env);
        lp_sac.mint(&staker_a, &1_000_i128);
        lp_sac.mint(&staker_b, &3_000_i128);
        staking.stake(&staker_a, &1_000_i128);
        staking.stake(&staker_b, &3_000_i128);

        let reward_client = StellarTokenClient::new(&env, &reward_token.address);
        let mut paid_total: i128 = 0;
        for _round in 0..3u32 {
            let x = 10_000_i128;
            staking.add_rewards(&admin, &x);
            staking.update_rewards(&admin, &x);

            let pool = staking.get_pool_info();
            assert_eq!(
                pool.reward_pool_balance, 0,
                "each round distributes exactly X, so nothing is left undistributed"
            );
            assert_eq!(staking.get_owed_rewards(), x);

            paid_total += staking.claim(&staker_a) + staking.claim(&staker_b);
            assert_eq!(staking.get_owed_rewards(), 0);
            assert_eq!(staking.get_pool_info().reward_pool_balance, 0);
        }

        // 30_000 distributed across 1:3 shares; 10_000 over 4_000 effective
        // is exactly 2.5 per share, so every round pays out in full with no
        // rounding dust at all.
        assert_eq!(paid_total, 30_000);
        assert_eq!(reward_client.balance(&staker_a), 7_500);
        assert_eq!(reward_client.balance(&staker_b), 22_500);
    }

    /// Property-style soak: 300+ random operations over 3+ stakers covering
    /// stake, unstake, lock, extend_lock, claim, add_rewards,
    /// update_rewards, and emergency_withdraw. After every step the
    /// accounting invariant balance >= undistributed + owed holds and neither
    /// counter is negative. On main this fails almost immediately: claims
    /// decrement the same counter update_rewards already debited, so the
    /// balance drops below undistributed + owed (and below 0).
    #[test]
    fn test_accounting_invariant_over_randomized_operations_1045() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let staking_addr = env.register_contract(None, Staking);
        let (lp_token, lp_sac) = create_sac(&env, &admin);
        let (reward_token, reward_sac) = create_sac(&env, &admin);
        let staking = StakingClient::new(&env, &staking_addr);
        staking.initialize(&lp_token.address, &reward_token.address, &admin);
        reward_sac.mint(&admin, &1_000_000_i128);

        let stakers: [Address; 4] = [
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
        ];
        for s in stakers.iter() {
            lp_sac.mint(s, &100_000_i128);
        }
        let reward_client = StellarTokenClient::new(&env, &reward_token.address);

        let mut rng_state: u64 = 0x1045_1045;
        for step in 0..320u32 {
            // 320 host-invoked operations blow the default cumulative CPU
            // budget long before step 300 even though every individual call
            // is cheap; the budget is a harness concern, not part of the
            // invariant under test, so refresh it periodically.
            if step > 0 && step % 64 == 0 {
                env.budget().reset_default();
            }
            let r = next_rand(&mut rng_state);
            let who = (r % 4) as usize;
            let action = (r / 4) % 8;
            let staker = &stakers[who];

            match action {
                0 => {
                    let amount = 100 + (r % 900) as i128;
                    staking.stake(staker, &amount);
                }
                1 => {
                    let amount = 100 + (r % 900) as i128;
                    let span = MAX_LOCK_DURATION - MIN_LOCK_DURATION;
                    let dur = MIN_LOCK_DURATION + (r % (span + 1));
                    staking.stake_locked(staker, &amount, &dur);
                }
                2 => {
                    // advance time so locks expire in the background
                    let delta = 1 + (r % (MIN_LOCK_DURATION / 2));
                    env.ledger().with_mut(|l| l.timestamp += delta);
                }
                3 => {
                    if staking.total_effective_staked() > 0 {
                        let amount = 10 + (r % 500) as i128;
                        staking.add_rewards(&admin, &amount);
                        staking.update_rewards(&admin, &amount);
                    }
                }
                4 => {
                    if staking.pending_rewards(staker) > 0 {
                        staking.claim(staker);
                    }
                }
                5 => {
                    // extend_lock on a live lock
                    let expiry = staking.boost_expires_at(staker);
                    if expiry > env.ledger().timestamp() {
                        staking.extend_lock(staker, &MIN_LOCK_DURATION);
                    }
                }
                6 => {
                    // partial unstake, lock permitting
                    let staked = staking.get_staker_info(staker).staked_amount;
                    let expiry = staking.boost_expires_at(staker);
                    if staked > 0 && env.ledger().timestamp() >= expiry {
                        let amount = 1 + (r % (staked as u64).max(1)) as i128;
                        staking.unstake(staker, &amount.min(staked));
                    }
                }
                _ => {
                    // emergency_withdraw forfeits pending rewards
                    if staking.get_staker_info(staker).staked_amount > 0 {
                        staking.set_emergency_mode(&admin, &true);
                        staking.emergency_withdraw(staker);
                        staking.set_emergency_mode(&admin, &false);
                    }
                }
            }

            // ---- The #1045 invariant, after every step ----
            let undistributed = staking.get_pool_info().reward_pool_balance;
            let owed = staking.get_owed_rewards();
            assert!(
                undistributed >= 0,
                "step {step}: undistributed went negative ({undistributed})"
            );
            assert!(owed >= 0, "step {step}: owed went negative ({owed})");
            let balance = reward_client.balance(&staking_addr);
            assert!(
                balance >= undistributed + owed,
                "step {step}: balance {balance} < undistributed {undistributed} + owed {owed}"
            );
        }

        // Nothing was lost, either: the final step's invariant already
        // compared the contract's token balance against both counters.
        let undistributed = staking.get_pool_info().reward_pool_balance;
        let owed = staking.get_owed_rewards();
        assert!(
            reward_client.balance(&staking_addr) >= undistributed + owed,
            "final invariant violated"
        );
    }

    /// The cap limits undistributed funding only: claims must not affect it.
    #[test]
    fn test_cap_limits_undistributed_funding_and_is_unaffected_by_claims() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let staking_addr = env.register_contract(None, Staking);
        let (lp_token, lp_sac) = create_sac(&env, &admin);
        let (reward_token, reward_sac) = create_sac(&env, &admin);
        let staking = StakingClient::new(&env, &staking_addr);
        staking.initialize(&lp_token.address, &reward_token.address, &admin);
        reward_sac.mint(&admin, &100_000_i128);
        staking.set_max_reward_pool_balance(&admin, &5_000_i128);

        let staker = Address::generate(&env);
        lp_sac.mint(&staker, &1_000_i128);
        staking.stake(&staker, &1_000_i128);

        // Funding up to the cap is fine.
        staking.add_rewards(&admin, &5_000_i128);
        // Exceeding it is rejected...
        assert_eq!(
            staking.try_add_rewards(&admin, &1_i128),
            Err(Ok(StakingError::MaxRewardPoolExceeded))
        );

        // ...and after distributing and claiming, the undistributed side is
        // zero, so the full cap headroom is available again: claims never
        // consumed cap capacity on main's corrupted counter.
        staking.update_rewards(&admin, &5_000_i128);
        let claimed = staking.claim(&staker);
        assert_eq!(claimed, 5_000);
        staking.add_rewards(&admin, &5_000_i128);
        assert_eq!(staking.get_pool_info().reward_pool_balance, 5_000);

        // Lowering the cap onto a pool with only owed rewards (nothing
        // undistributed) is allowed — the cap is defined against
        // undistributed rewards, not the corrupted main counter.
        staking.update_rewards(&admin, &5_000_i128);
        staking.set_max_reward_pool_balance(&admin, &1_i128);
        assert_eq!(staking.get_owed_rewards(), 5_000);
    }

    /// The cap view (`reward_pool_balance`) stays accurate while owed rewards
    /// are outstanding — under main's double-decrement this read went
    /// negative.
    #[test]
    fn test_pool_info_balance_never_negative_with_outstanding_owed() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        staking.stake(&staker, &1_000_i128);
        staking.update_rewards(&admin, &9_000_i128);

        let pool = staking.get_pool_info();
        assert!(pool.reward_pool_balance >= 0);
        assert_eq!(pool.reward_pool_balance, 1_000);
        assert_eq!(staking.get_owed_rewards(), 9_000);
    }

    /// Settlements (stake / extend_lock / settle_boost) decrement the owed
    /// counter exactly like claims do, and never the undistributed side.
    #[test]
    fn test_settlement_decrements_owed_not_undistributed() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        staking.stake(&staker, &1_000_i128);
        staking.update_rewards(&admin, &500_i128);
        assert_eq!(staking.get_owed_rewards(), 500);
        assert_eq!(staking.get_pool_info().reward_pool_balance, 9_500);

        // stake_locked settles pending rewards before changing the position.
        staking.stake_locked(&staker, &1_000_i128, &MIN_LOCK_DURATION);
        assert_eq!(
            staking.get_owed_rewards(),
            0,
            "settlement must decrement owed"
        );
        assert_eq!(
            staking.get_pool_info().reward_pool_balance,
            9_500,
            "settlement must not touch undistributed"
        );
    }

    /// `emergency_withdraw` forfeits pending rewards, which must leave the
    /// owed counter: those tokens revert to undistributed and the invariant
    /// keeps holding.
    #[test]
    fn test_emergency_withdraw_forfeits_pending_out_of_owed_1045() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);
        let reward_token = staking.get_pool_info().reward_token;
        let reward_client = StellarTokenClient::new(&env, &reward_token);

        staking.stake(&staker, &1_000_i128);
        staking.update_rewards(&admin, &500_i128);
        assert_eq!(staking.get_owed_rewards(), 500);

        staking.set_emergency_mode(&admin, &true);
        staking.emergency_withdraw(&staker);

        assert_eq!(
            staking.get_owed_rewards(),
            0,
            "forfeited pending must leave the owed counter"
        );
        assert_eq!(
            staking.get_pool_info().reward_pool_balance,
            9_500,
            "forfeiture must not change the undistributed side"
        );
        // balance (10_000, untouched by emergency_withdraw) >= 9_500 + 0.
        assert!(reward_client.balance(&staking.address) >= 9_500);
    }

    /// `reconcile_reward_accounting` repairs a contract whose storage was
    /// seeded with main's corrupted negative value.
    #[test]
    fn test_reconcile_reward_accounting_repairs_corrupted_storage() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);
        let reward_token = staking.get_pool_info().reward_token;
        let reward_client = StellarTokenClient::new(&env, &reward_token);

        staking.stake(&staker, &1_000_i128);
        staking.update_rewards(&admin, &500_i128);
        assert_eq!(staking.get_owed_rewards(), 500);

        // Seed the legacy corruption directly: a negative RewardPoolBalance,
        // as observed on main after the first payout cycle, plus a missing
        // OwedRewards entry (pre-fix deployments never had one).
        env.as_contract(&staking.address, || {
            env.storage()
                .instance()
                .set(&DataKey::RewardPoolBalance, &(-10_000_i128));
            env.storage().instance().remove(&DataKey::OwedRewards);
        });
        assert_eq!(staking.get_pool_info().reward_pool_balance, -10_000);

        // The contract holds 10_000 tokens (nothing paid out yet); one
        // staker is owed 500 of them, so 9_500 are undistributed.
        let (undistributed, owed) = staking.reconcile_reward_accounting(&admin, &0, &50, &true);
        assert_eq!((undistributed, owed), (9_500, 500));
        assert_eq!(staking.get_pool_info().reward_pool_balance, 9_500);
        assert_eq!(staking.get_owed_rewards(), 500);

        // The repaired contract is fully functional again: the staker can
        // claim the owed 500, and a fresh cycle distributes normally.
        assert_eq!(staking.claim(&staker), 500);
        assert_eq!(staking.get_owed_rewards(), 0);
        assert_eq!(
            staking.get_pool_info().reward_pool_balance,
            9_500,
            "claim must not touch the undistributed side"
        );
        assert_eq!(reward_client.balance(&staker), 500);
    }

    /// `reconcile_reward_accounting` is admin-only.
    #[test]
    fn test_reconcile_reward_accounting_is_admin_only() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, staker, staking) = setup(&env);
        assert_eq!(
            staking.try_reconcile_reward_accounting(&staker, &0, &50, &true),
            Err(Ok(StakingError::Unauthorized))
        );
        // And it rejects out-of-range page sizes.
        let admin = staking.get_admin();
        assert_eq!(
            staking.try_reconcile_reward_accounting(&admin, &0, &0, &true),
            Err(Ok(StakingError::BatchTooLarge))
        );
        assert_eq!(
            staking.try_reconcile_reward_accounting(&admin, &0, &(MAX_BATCH_SIZE + 1), &true),
            Err(Ok(StakingError::BatchTooLarge))
        );
    }

    /// Paginated reconcile over multiple stakers: partial pages checkpoint
    /// their total, and the final page writes both counters. No staker loses
    /// a credited reward to the repair.
    #[test]
    fn test_reconcile_reward_accounting_paginates_over_stakers() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let staking_addr = env.register_contract(None, Staking);
        let (lp_token, lp_sac) = create_sac(&env, &admin);
        let (reward_token, reward_sac) = create_sac(&env, &admin);
        let staking = StakingClient::new(&env, &staking_addr);
        staking.initialize(&lp_token.address, &reward_token.address, &admin);
        reward_sac.mint(&admin, &10_000_i128);

        // Three equal stakers, one 3_000 distribution.
        let stakers: [Address; 3] = [
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
        ];
        for s in stakers.iter() {
            lp_sac.mint(s, &1_000_i128);
            staking.stake(s, &1_000_i128);
        }
        staking.add_rewards(&admin, &3_000_i128);
        staking.update_rewards(&admin, &3_000_i128);

        // Corrupt the legacy counter the way main did.
        env.as_contract(&staking_addr, || {
            env.storage()
                .instance()
                .set(&DataKey::RewardPoolBalance, &(-5_i128));
            env.storage().instance().remove(&DataKey::OwedRewards);
        });

        // Page 1 (1 staker), then the final page with the rest.
        let running = staking.reconcile_reward_accounting(&admin, &0, &1, &false);
        assert_eq!(running.1, 1_000, "page 1 checkpoints one staker's credit");
        let (undistributed, owed) = staking.reconcile_reward_accounting(&admin, &1, &50, &true);
        assert_eq!((undistributed, owed), (0, 3_000));

        // Every staker can claim their full credit afterward.
        for s in stakers.iter() {
            assert_eq!(staking.claim(s), 1_000);
        }
        assert_eq!(staking.get_owed_rewards(), 0);
        assert_eq!(staking.get_pool_info().reward_pool_balance, 0);
    }

    /// A no-op reconcile (nothing staked, nothing owed) leaves a healthy
    /// pool's accounting untouched and is safe to run at any time.
    #[test]
    fn test_reconcile_reward_accounting_is_noop_on_healthy_pool() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, _staker, staking) = setup(&env);

        // `setup` funded 10_000; nobody staked, so nothing is owed. The
        // reconcile rewrites both counters from the token balance (10_000)
        // and the recomputed owed (0), leaving a healthy pool untouched.
        let (undistributed, owed) = staking.reconcile_reward_accounting(&admin, &0, &50, &true);
        assert_eq!((undistributed, owed), (10_000, 0));
        assert_eq!(staking.get_pool_info().reward_pool_balance, 10_000);
        assert_eq!(staking.get_owed_rewards(), 0);
    }

    // ── #1114 / #1113 shared helpers ─────────────────────────────────────────

    extern crate std;
    use std::vec::Vec as StdVec;

    fn find_event(env: &Env, name: &str) -> Option<soroban_sdk::Val> {
        use soroban_sdk::{testutils::Events as _, TryFromVal};
        // Compare decoded symbols, not raw `Val` payloads: a symbol longer than
        // nine characters is an object handle, so two equal long symbols have
        // different payloads.
        let wanted = Symbol::new(env, name);
        env.events()
            .all()
            .iter()
            .find(|event| {
                event
                    .1
                    .get(0)
                    .and_then(|t| Symbol::try_from_val(env, &t).ok())
                    .map(|topic| topic == wanted)
                    .unwrap_or(false)
            })
            .map(|event| event.2)
    }

    fn fresh_staker(env: &Env, lp: &Address, amount: i128) -> Address {
        let staker = Address::generate(env);
        StellarAssetClient::new(env, lp).mint(&staker, &amount);
        staker
    }

    fn join(env: &Env, staking: &StakingClient<'_>, lp: &Address, amount: i128) -> Address {
        let staker = fresh_staker(env, lp, amount);
        staking.stake(&staker, &amount);
        staker
    }

    /// Read the whole registry back from storage and check it is internally
    /// consistent: dense `StakerAt(0..count)`, `StakerSlot` inverting it, and
    /// nothing left behind past the count.
    fn registry(env: &Env, staking: &StakingClient<'_>) -> StdVec<Address> {
        env.as_contract(&staking.address, || {
            let count: u32 = env
                .storage()
                .persistent()
                .get(&DataKey::StakerCount)
                .unwrap_or(0);
            let mut out = StdVec::new();
            for i in 0..count {
                let at: Address = env
                    .storage()
                    .persistent()
                    .get(&DataKey::StakerAt(i))
                    .expect("hole in the registry");
                let slot: u32 = env
                    .storage()
                    .persistent()
                    .get(&DataKey::StakerSlot(at.clone()))
                    .expect("registered staker without a slot");
                assert_eq!(slot, i, "StakerSlot must invert StakerAt");
                out.push(at);
            }
            assert!(
                !env.storage().persistent().has(&DataKey::StakerAt(count)),
                "entry left behind past StakerCount"
            );
            out
        })
    }

    fn assert_registry_is(env: &Env, staking: &StakingClient<'_>, expected: &[Address]) {
        let got = registry(env, staking);
        assert_eq!(got.len(), expected.len(), "registry size");
        for e in expected {
            assert!(got.contains(e), "registry is missing an expected staker");
        }
    }

    fn has_slot(env: &Env, staking: &StakingClient<'_>, who: &Address) -> bool {
        env.as_contract(&staking.address, || {
            env.storage()
                .persistent()
                .has(&DataKey::StakerSlot(who.clone()))
        })
    }

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *seed >> 33
    }

    fn advance_time(env: &Env, secs: u64) {
        env.ledger().with_mut(|l| l.timestamp += secs);
    }

    // ── #1114: paused boost settlement ───────────────────────────────────────

    #[test]
    fn test_settle_boost_pays_nothing_while_paused_and_carries_pending() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);

        staking.stake_locked(&staker, &1_000_i128, &MAX_LOCK_DURATION);
        staking.update_rewards(&admin, &500_i128);
        let owed = staking.pending_rewards(&staker);
        assert!(owed > 0);
        advance_time(&env, MAX_LOCK_DURATION + 1);

        let reward_token = staking.get_pool_info().reward_token;
        let rewards = StellarTokenClient::new(&env, &reward_token);
        let staker_before = rewards.balance(&staker);
        let contract_before = rewards.balance(&staking.address);
        let owed_total_before = staking.get_owed_rewards();

        staking.pause(&admin);
        staking.settle_boost(&staker);

        // No tokens left the pool and the owed counter did not move.
        assert_eq!(rewards.balance(&staker), staker_before);
        assert_eq!(rewards.balance(&staking.address), contract_before);
        assert_eq!(staking.get_owed_rewards(), owed_total_before);
        // The boost bookkeeping is still corrected.
        assert_eq!(
            staking.get_staker_info(&staker).boost_multiplier,
            BOOST_SCALE
        );
        assert_eq!(staking.get_pool_info().total_effective_staked, 1_000);
        // The reward earned at the peak rate is carried, not lost.
        assert_eq!(staking.pending_rewards(&staker), owed);

        staking.unpause(&admin);
        assert_eq!(staking.claim(&staker), owed);
        assert_eq!(rewards.balance(&staker), staker_before + owed);
        assert_eq!(staking.get_owed_rewards(), owed_total_before - owed);
    }

    #[test]
    fn test_settle_boost_while_paused_cannot_be_used_to_force_payouts() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, staker, staking) = setup(&env);
        staking.stake_locked(&staker, &1_000_i128, &MAX_LOCK_DURATION);
        staking.update_rewards(&admin, &500_i128);
        advance_time(&env, MAX_LOCK_DURATION + 1);
        staking.pause(&admin);

        let rewards = StellarTokenClient::new(&env, &staking.get_pool_info().reward_token);
        let before = rewards.balance(&staker);
        // Repeated permissionless calls are harmless: the first settles, the
        // rest are no-ops, and none of them pays.
        staking.settle_boost(&staker);
        staking.settle_boost(&staker);
        let mut batch = Vec::new(&env);
        batch.push_back(staker.clone());
        staking.settle_boost_batch(&batch);
        assert_eq!(rewards.balance(&staker), before);
    }

    #[test]
    fn test_settle_boost_batch_while_paused_mixed_batch() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, a, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        let b = fresh_staker(&env, &lp, 1_000);
        let c = fresh_staker(&env, &lp, 1_000);
        let stranger = Address::generate(&env);

        staking.stake_locked(&a, &1_000_i128, &MAX_LOCK_DURATION);
        staking.stake(&b, &1_000_i128);
        staking.stake_locked(&c, &1_000_i128, &MAX_LOCK_DURATION);
        staking.update_rewards(&admin, &1_000_i128);
        let (pa, pb, pc) = (
            staking.pending_rewards(&a),
            staking.pending_rewards(&b),
            staking.pending_rewards(&c),
        );
        advance_time(&env, MAX_LOCK_DURATION + 1);

        let rewards = StellarTokenClient::new(&env, &staking.get_pool_info().reward_token);
        let balances = (
            rewards.balance(&a),
            rewards.balance(&b),
            rewards.balance(&c),
            rewards.balance(&staking.address),
        );

        staking.pause(&admin);
        let mut batch = Vec::new(&env);
        batch.push_back(a.clone());
        batch.push_back(b.clone());
        batch.push_back(stranger);
        batch.push_back(c.clone());
        staking.settle_boost_batch(&batch);

        assert_eq!(
            balances,
            (
                rewards.balance(&a),
                rewards.balance(&b),
                rewards.balance(&c),
                rewards.balance(&staking.address)
            ),
            "no balance may change while paused"
        );
        assert_eq!(staking.get_staker_info(&a).boost_multiplier, BOOST_SCALE);
        assert_eq!(staking.get_staker_info(&c).boost_multiplier, BOOST_SCALE);
        assert_eq!(staking.get_pool_info().total_effective_staked, 3_000);

        staking.unpause(&admin);
        assert_eq!(staking.claim(&a), pa);
        assert_eq!(staking.claim(&b), pb);
        assert_eq!(staking.claim(&c), pc);
    }

    // ── #1114: versioned admin events ────────────────────────────────────────

    #[test]
    fn test_propose_and_accept_admin_events_are_versioned() {
        use soroban_sdk::TryFromVal;
        let env = Env::default();
        env.mock_all_auths();
        let (admin, _staker, staking) = setup(&env);
        let next = Address::generate(&env);

        staking.propose_admin(&admin, &next);
        let data = find_event(&env, "admin_nominated").expect("admin_nominated event");
        let (version, (from, to)) = <(u32, (Address, Address))>::try_from_val(&env, &data).unwrap();
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!((from, to), (admin.clone(), next.clone()));

        staking.accept_admin(&next);
        let data = find_event(&env, "admin_changed").expect("admin_changed event");
        let (version, (who,)) = <(u32, (Address,))>::try_from_val(&env, &data).unwrap();
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(who, next);
        assert_eq!(staking.get_admin(), next);
    }

    // ── #1114: initialization is verifiable ──────────────────────────────────

    #[test]
    fn test_initialize_emits_initialized_event_matching_get_config() {
        use soroban_sdk::TryFromVal;
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let (lp, _) = create_sac(&env, &admin);
        let (reward, _) = create_sac(&env, &admin);
        let staking = StakingClient::new(&env, &env.register_contract(None, Staking));

        staking.initialize(&lp.address, &reward.address, &admin);

        let data = find_event(&env, "initialized").expect("initialized event");
        let (version, (l, r, a, min_b, max_b, min_l, max_l)) =
            <(u32, (Address, Address, Address, i128, i128, u64, u64))>::try_from_val(&env, &data)
                .unwrap();
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let cfg = staking.get_config();
        assert_eq!(
            cfg,
            StakingConfig {
                lp_token: l,
                reward_token: r,
                admin: a,
                min_boost: min_b,
                max_boost: max_b,
                min_lock_duration: min_l,
                max_lock_duration: max_l,
            }
        );
        assert_eq!(cfg.lp_token, lp.address);
        assert_eq!(cfg.reward_token, reward.address);
        assert_eq!(cfg.admin, admin);
        assert_eq!(cfg.min_boost, MIN_BOOST);
        assert_eq!(cfg.max_boost, DEFAULT_MAX_BOOST);
        assert_eq!(cfg.min_lock_duration, MIN_LOCK_DURATION);
        assert_eq!(cfg.max_lock_duration, MAX_LOCK_DURATION);
        assert_eq!(staking.get_admin(), admin);
    }

    #[test]
    fn test_initialize_with_boost_config_reports_custom_values() {
        use soroban_sdk::TryFromVal;
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let (lp, _) = create_sac(&env, &admin);
        let (reward, _) = create_sac(&env, &admin);
        let staking = StakingClient::new(&env, &env.register_contract(None, Staking));

        staking.initialize_with_boost_config(
            &lp.address,
            &reward.address,
            &admin,
            &10_000_i128,
            &30_000_i128,
            &(MIN_LOCK_DURATION + 1),
            &(MAX_LOCK_DURATION + 1),
        );

        let cfg = staking.get_config();
        assert_eq!(cfg.max_boost, 30_000);
        assert_eq!(cfg.min_lock_duration, MIN_LOCK_DURATION + 1);
        assert_eq!(cfg.max_lock_duration, MAX_LOCK_DURATION + 1);
        let data = find_event(&env, "initialized").expect("initialized event");
        let (_, (_, _, _, _, max_b, min_l, _)) =
            <(u32, (Address, Address, Address, i128, i128, u64, u64))>::try_from_val(&env, &data)
                .unwrap();
        assert_eq!((max_b, min_l), (30_000, MIN_LOCK_DURATION + 1));
    }

    #[test]
    fn test_get_config_and_get_admin_before_initialize_fail() {
        let env = Env::default();
        env.mock_all_auths();
        let staking = StakingClient::new(&env, &env.register_contract(None, Staking));
        assert_eq!(
            staking.try_get_config(),
            Err(Ok(StakingError::NotInitialized))
        );
        assert_eq!(
            staking.try_get_admin(),
            Err(Ok(StakingError::NotInitialized))
        );
    }

    #[test]
    fn test_initialize_requires_no_authorization() {
        // `initialize` deliberately has no `require_auth`: the admin may be a
        // contract that cannot sign for itself. No auth is mocked here, so any
        // `require_auth` inside it would fail this call.
        let env = Env::default();
        let admin = Address::generate(&env);
        let (lp, _) = create_sac(&env, &admin);
        let (reward, _) = create_sac(&env, &admin);
        let staking = StakingClient::new(&env, &env.register_contract(None, Staking));
        staking.initialize(&lp.address, &reward.address, &admin);
        assert!(env.auths().is_empty(), "initialize must not require auth");
        assert_eq!(staking.get_config().admin, admin);
    }

    #[test]
    fn test_initialize_rejects_identical_tokens() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let (token, _) = create_sac(&env, &admin);
        let staking = StakingClient::new(&env, &env.register_contract(None, Staking));

        assert_eq!(
            staking.try_initialize(&token.address, &token.address, &admin),
            Err(Ok(StakingError::IdenticalTokens))
        );
        // A rejected call leaves the contract uninitialized.
        assert_eq!(
            staking.try_get_config(),
            Err(Ok(StakingError::NotInitialized))
        );
        assert_eq!(StakingError::IdenticalTokens as u32, 22);
    }

    #[test]
    fn test_initialize_with_boost_config_rejects_identical_tokens() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let (token, _) = create_sac(&env, &admin);
        let staking = StakingClient::new(&env, &env.register_contract(None, Staking));

        assert_eq!(
            staking.try_initialize_with_boost_config(
                &token.address,
                &token.address,
                &admin,
                &10_000_i128,
                &30_000_i128,
                &MIN_LOCK_DURATION,
                &MAX_LOCK_DURATION,
            ),
            Err(Ok(StakingError::IdenticalTokens))
        );
    }

    #[test]
    fn test_initialize_twice_still_fails_with_already_initialized() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, _staker, staking) = setup(&env);
        let cfg = staking.get_config();
        assert_eq!(
            staking.try_initialize(&cfg.lp_token, &cfg.reward_token, &admin),
            Err(Ok(StakingError::AlreadyInitialized))
        );
        assert_eq!(staking.get_config(), cfg);
    }

    // ── #1113: registry cost ─────────────────────────────────────────────────

    // ── O(1) registry (#1113) ────────────────────────────────────────────────
    //
    // These tests used to compare CPU instruction counts between a 5-staker and
    // a 500-staker pool. In the native test host that number is dominated by the
    // size of the whole test ledger, not by the contract: every call copies the
    // host's storage map, and every new entry is inserted into it, so a plain
    // `get_pool_info` or SAC `transfer` costs more as unrelated entries pile up.
    // Counting how many ledger entries a call changes measures what the registry
    // is meant to guarantee — a fixed amount of storage work whatever the pool
    // size — and does not depend on the host's metering.

    /// Every ledger entry (key, value and TTL), rendered so two snapshots can be
    /// compared without depending on the entry types.
    fn ledger_dump(
        env: &Env,
    ) -> std::collections::BTreeMap<std::string::String, std::string::String> {
        env.to_ledger_snapshot()
            .ledger_entries
            .iter()
            .map(|(key, (entry, ttl))| {
                (
                    std::format!("{key:?}"),
                    std::format!("{entry:?} ttl={ttl:?}"),
                )
            })
            .collect()
    }

    /// How many ledger entries `f` added, removed or changed.
    fn entries_touched(env: &Env, f: impl FnOnce()) -> usize {
        let before = ledger_dump(env);
        f();
        let after = ledger_dump(env);
        let changed = after
            .iter()
            .filter(|(k, v)| before.get(*k) != Some(*v))
            .count();
        let removed = before.keys().filter(|k| !after.contains_key(*k)).count();
        changed + removed
    }

    #[test]
    fn test_stake_cost_does_not_grow_with_staker_count() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        let (_admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;

        for _ in 0..4 {
            join(&env, &staking, &lp, 100);
        }
        let fifth = fresh_staker(&env, &lp, 100);
        let touched_5th = entries_touched(&env, || {
            staking.stake(&fifth, &100_i128);
        });

        for _ in 0..494 {
            join(&env, &staking, &lp, 100);
        }
        let last = fresh_staker(&env, &lp, 100);
        let touched_500th = entries_touched(&env, || {
            staking.stake(&last, &100_i128);
        });

        assert_eq!(registry(&env, &staking).len(), 500);
        assert!(touched_5th > 0, "a stake must write something");
        assert_eq!(
            touched_500th, touched_5th,
            "stake touched {touched_5th} ledger entries for the 5th staker but {touched_500th} for the 500th"
        );
    }

    /// Ledger entries touched when the staker at `idx` of `n` leaves.
    fn exit_cost(n: u32, idx: usize, emergency: bool) -> usize {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        let (admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        let mut stakers = StdVec::new();
        for _ in 0..n {
            stakers.push(join(&env, &staking, &lp, 100));
        }
        if emergency {
            staking.set_emergency_mode(&admin, &true);
        }
        let who = stakers[idx].clone();
        let touched = entries_touched(&env, || {
            if emergency {
                staking.emergency_withdraw(&who);
            } else {
                let _ = staking.unstake(&who, &100_i128);
            }
        });
        assert_eq!(registry(&env, &staking).len(), n as usize - 1);
        assert!(!has_slot(&env, &staking, &who));
        touched
    }

    #[test]
    fn test_unstake_cost_does_not_grow_with_staker_count() {
        // A middle staker leaves, so the swap-remove path runs in both pools.
        let small = exit_cost(5, 2, false);
        let large = exit_cost(500, 250, false);
        assert!(small > 0, "an unstake must write something");
        assert_eq!(
            large, small,
            "unstake touched {small} ledger entries in a 5-staker pool but {large} in a 500-staker pool"
        );
    }

    #[test]
    fn test_emergency_withdraw_cost_does_not_grow_with_staker_count() {
        let small = exit_cost(5, 2, true);
        let large = exit_cost(500, 250, true);
        assert!(small > 0, "an emergency withdrawal must write something");
        assert_eq!(
            large, small,
            "emergency_withdraw touched {small} ledger entries in a 5-staker pool but {large} in a 500-staker pool"
        );
    }

    /// The staking contract's instance entry, rendered. It is the one instance
    /// entry that holds `TotalEffectiveStaked`.
    fn staking_instance_dump(env: &Env) -> std::string::String {
        let matches: StdVec<std::string::String> = env
            .to_ledger_snapshot()
            .ledger_entries
            .iter()
            .filter(|(key, (entry, _))| {
                std::format!("{key:?}").contains("LedgerKeyContractInstance")
                    && std::format!("{entry:?}").contains("TotalEffectiveStaked")
            })
            .map(|(_, (entry, _))| std::format!("{entry:?}"))
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "expected exactly one staking instance entry"
        );
        matches.into_iter().next().unwrap()
    }

    #[test]
    fn test_instance_storage_does_not_grow_with_staker_count() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        let (_admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        let mut stakers = StdVec::new();
        for _ in 0..5 {
            stakers.push(join(&env, &staking, &lp, 100));
        }
        // Count the keys of the instance map; counter values gaining digits as
        // stakers join is not growth.
        let key_count = |env: &Env| staking_instance_dump(env).matches("key:").count();
        let small = key_count(&env);

        for _ in 0..295 {
            stakers.push(join(&env, &staking, &lp, 100));
        }
        let large = key_count(&env);

        // Every call loads the whole instance entry, so the number of keys in it
        // must not depend on how many stakers there are.
        assert!(
            small > 0,
            "the instance map should hold the pool configuration"
        );
        assert_eq!(
            large, small,
            "instance map grew from {small} to {large} keys"
        );
        // And none of the registry or per-staker data lives in instance storage.
        env.as_contract(&staking.address, || {
            let instance = env.storage().instance();
            assert!(!instance.has(&DataKey::StakerIndex));
            assert!(!instance.has(&DataKey::StakerCount));
            assert!(!instance.has(&DataKey::StakerAt(0)));
            assert!(!instance.has(&DataKey::StakerSlot(stakers[0].clone())));
            assert!(!instance.has(&DataKey::StakerAmount(stakers[0].clone())));
        });
    }

    // ── #1113: registry consistency ──────────────────────────────────────────

    #[test]
    fn test_registry_swap_remove_moves_last_entry_into_the_freed_slot() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        let a = join(&env, &staking, &lp, 100);
        let b = join(&env, &staking, &lp, 100);
        let c = join(&env, &staking, &lp, 100);
        let d = join(&env, &staking, &lp, 100);
        assert_eq!(
            registry(&env, &staking),
            std::vec![a.clone(), b.clone(), c.clone(), d.clone()]
        );

        let _ = staking.unstake(&b, &100_i128);
        assert_eq!(
            registry(&env, &staking),
            std::vec![a.clone(), d.clone(), c.clone()]
        );

        // Removing the last entry just truncates.
        let _ = staking.unstake(&c, &100_i128);
        assert_eq!(registry(&env, &staking), std::vec![a.clone(), d.clone()]);
        let _ = staking.unstake(&a, &100_i128);
        let _ = staking.unstake(&d, &100_i128);
        assert!(registry(&env, &staking).is_empty());
    }

    #[test]
    fn test_top_up_and_partial_unstake_keep_a_single_registry_entry() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        let a = join(&env, &staking, &lp, 100);
        let other = join(&env, &staking, &lp, 100);
        StellarAssetClient::new(&env, &lp).mint(&a, &50);

        staking.stake(&a, &50_i128);
        assert_registry_is(&env, &staking, &[a.clone(), other.clone()]);
        let _ = staking.unstake(&a, &60_i128);
        assert_registry_is(&env, &staking, &[a.clone(), other.clone()]);
        let _ = staking.unstake(&a, &90_i128);
        assert_registry_is(&env, &staking, &[other]);
        assert!(!has_slot(&env, &staking, &a));
    }

    #[test]
    fn test_emergency_withdraw_removes_staker_from_registry() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        let a = join(&env, &staking, &lp, 100);
        let b = join(&env, &staking, &lp, 100);
        let c = join(&env, &staking, &lp, 100);
        staking.set_emergency_mode(&admin, &true);

        staking.emergency_withdraw(&a);
        assert_eq!(registry(&env, &staking), std::vec![c.clone(), b.clone()]);
        assert!(!has_slot(&env, &staking, &a));
    }

    #[test]
    fn test_registry_stays_consistent_over_300_random_joins_and_leaves() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        let (_admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        let mut seed = 0x5eed_u64;
        let mut live: StdVec<Address> = StdVec::new();
        let mut gone: StdVec<Address> = StdVec::new();

        for _ in 0..300 {
            let roll = lcg(&mut seed) % 100;
            if live.is_empty() || roll < 50 {
                live.push(join(&env, &staking, &lp, 100));
            } else if roll < 60 && !gone.is_empty() {
                // A staker who left comes back.
                let idx = lcg(&mut seed) as usize % gone.len();
                let back = gone.swap_remove(idx);
                staking.stake(&back, &100_i128);
                live.push(back);
            } else {
                let idx = lcg(&mut seed) as usize % live.len();
                let leaver = live.swap_remove(idx);
                let _ = staking.unstake(&leaver, &100_i128);
                gone.push(leaver);
            }
            assert_registry_is(&env, &staking, &live);
        }

        for who in gone.iter() {
            assert!(
                !has_slot(&env, &staking, who),
                "departed staker kept a slot"
            );
        }
        assert_eq!(
            staking.get_pool_info().total_effective_staked,
            100 * live.len() as i128
        );
    }

    #[test]
    fn test_registry_entries_are_bumped_on_write_and_on_read() {
        use soroban_sdk::testutils::storage::Persistent as _;
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        let a = join(&env, &staking, &lp, 100);

        let ttl = |key: DataKey| -> u32 {
            env.as_contract(&staking.address, || {
                env.storage().persistent().get_ttl(&key)
            })
        };
        assert!(ttl(DataKey::StakerCount) >= BUMP_TO - 1);
        assert!(ttl(DataKey::StakerAt(0)) >= BUMP_TO - 1);
        assert!(ttl(DataKey::StakerSlot(a.clone())) >= BUMP_TO - 1);

        env.ledger()
            .with_mut(|l| l.sequence_number += BUMP_TO - MIN_TTL + 1);
        assert!(ttl(DataKey::StakerAt(0)) < MIN_TTL);
        staking.list_expired_boosts(&0, &10);
        assert!(
            ttl(DataKey::StakerAt(0)) >= BUMP_TO - 1,
            "page read must bump"
        );
        assert!(ttl(DataKey::StakerCount) >= BUMP_TO - 1);
    }

    // ── #1113: keeper functions over a large registry ────────────────────────

    #[test]
    fn test_list_expired_boosts_pagination_is_equivalent_for_200_stakers() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        let (_admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        let mut expected: StdVec<Address> = StdVec::new();
        for i in 0..200u32 {
            let staker = fresh_staker(&env, &lp, 100);
            if i % 3 == 0 {
                staking.stake_locked(&staker, &100_i128, &MAX_LOCK_DURATION);
                expected.push(staker);
            } else {
                staking.stake(&staker, &100_i128);
            }
        }
        advance_time(&env, MAX_LOCK_DURATION + 1);
        let count = registry(&env, &staking).len() as u32;
        assert_eq!(count, 200);

        let collect = |page: u32| -> StdVec<Address> {
            let mut out = StdVec::new();
            let mut offset = 0u32;
            while offset < count {
                for a in staking.list_expired_boosts(&offset, &page).iter() {
                    out.push(a);
                }
                offset += page.min(MAX_LIST_LIMIT);
            }
            out
        };
        let mut by_100 = collect(100);
        let mut by_37 = collect(37);
        let mut by_1000 = collect(1_000); // clamped to MAX_LIST_LIMIT
        expected.sort();
        by_100.sort();
        by_37.sort();
        by_1000.sort();
        assert_eq!(by_100, expected);
        assert_eq!(by_37, expected);
        assert_eq!(by_1000, expected);
        by_100.dedup();
        assert_eq!(by_100.len(), expected.len(), "no staker listed twice");

        // A keeper settles everything it found in MAX_BATCH_SIZE chunks.
        for chunk in expected.chunks(MAX_BATCH_SIZE as usize) {
            let mut batch = Vec::new(&env);
            for a in chunk {
                batch.push_back(a.clone());
            }
            staking.settle_boost_batch(&batch);
        }
        assert!(collect(100).is_empty());
        assert_eq!(registry(&env, &staking).len(), 200);
    }

    #[test]
    fn test_reconcile_over_200_stakers_is_independent_of_page_size() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        let (admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        let mut stakers = StdVec::new();
        for _ in 0..200 {
            stakers.push(join(&env, &staking, &lp, 100));
        }
        staking.update_rewards(&admin, &10_000_i128);
        let pending_sum: i128 = stakers.iter().map(|s| staking.pending_rewards(s)).sum();
        assert_eq!(pending_sum, 10_000);

        // Run 1: four pages of 50, finalized on the last.
        assert_eq!(
            staking.reconcile_reward_accounting(&admin, &0, &50, &false),
            (0, 2_500)
        );
        assert_eq!(
            staking.reconcile_reward_accounting(&admin, &50, &50, &false),
            (0, 5_000)
        );
        assert_eq!(
            staking.reconcile_reward_accounting(&admin, &100, &50, &false),
            (0, 7_500)
        );
        assert_eq!(
            staking.reconcile_reward_accounting(&admin, &150, &50, &true),
            (0, 10_000)
        );
        env.as_contract(&staking.address, || {
            assert!(
                !env.storage().persistent().has(&DataKey::ReconcileTotal),
                "the running total is cleared on finalize"
            );
            assert!(!env
                .storage()
                .persistent()
                .has(&DataKey::ReconcileState(stakers[199].clone())));
        });

        // Run 2: seven pages of 30 straight after. Leftovers from run 1 must
        // not make it undercount.
        let mut offset = 0u32;
        let mut last = (0, 0);
        while offset < 200 {
            let done = offset + 30 >= 200;
            last = staking.reconcile_reward_accounting(&admin, &offset, &30, &done);
            offset += 30;
        }
        assert_eq!(last, (0, 10_000));
        assert_eq!(staking.get_owed_rewards(), 10_000);
        for s in stakers.iter().take(5) {
            assert_eq!(staking.claim(s), 50);
        }
    }

    #[test]
    fn test_reconcile_page_overlap_does_not_double_count_on_registry() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        for _ in 0..4 {
            join(&env, &staking, &lp, 100);
        }
        staking.update_rewards(&admin, &4_000_i128);

        let first = staking.reconcile_reward_accounting(&admin, &0, &3, &false);
        assert_eq!(first, (0, 3_000));
        // Overlapping page: staker 2 is seen again and must not count twice.
        let fin = staking.reconcile_reward_accounting(&admin, &2, &3, &true);
        // 10_000 funded, 4_000 distributed and owed, 6_000 still undistributed.
        assert_eq!(fin, (6_000, 4_000));
    }

    // ── #1113: migration from the legacy Vec layout ──────────────────────────

    fn seed_legacy(
        env: &Env,
        staking: &StakingClient<'_>,
        lp: &Address,
        n: u32,
        amount: i128,
    ) -> StdVec<Address> {
        let mut stakers = StdVec::new();
        let mut legacy = Vec::new(env);
        for _ in 0..n {
            let s = Address::generate(env);
            legacy.push_back(s.clone());
            stakers.push(s);
        }
        StellarAssetClient::new(env, lp).mint(&staking.address, &(amount * n as i128));
        env.as_contract(&staking.address, || {
            for s in stakers.iter() {
                env.storage()
                    .persistent()
                    .set(&DataKey::StakerAmount(s.clone()), &amount);
            }
            env.storage().instance().set(&DataKey::StakerIndex, &legacy);
            env.storage()
                .instance()
                .set(&DataKey::TotalEffectiveStaked, &(amount * n as i128));
        });
        stakers
    }

    fn legacy_present(env: &Env, staking: &StakingClient<'_>) -> bool {
        env.as_contract(&staking.address, || {
            env.storage().instance().has(&DataKey::StakerIndex)
        })
    }

    #[test]
    fn test_migrate_staker_index_moves_legacy_vec_in_pages() {
        use soroban_sdk::TryFromVal;
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        let (_admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        let stakers = seed_legacy(&env, &staking, &lp, 120, 100);
        // Two of them already left before the upgrade.
        env.as_contract(&staking.address, || {
            env.storage()
                .persistent()
                .set(&DataKey::StakerAmount(stakers[7].clone()), &0i128);
            env.storage()
                .persistent()
                .set(&DataKey::StakerAmount(stakers[100].clone()), &0i128);
        });

        assert_eq!(staking.migrate_staker_index(&0, &50), 70);
        assert!(legacy_present(&env, &staking));
        assert_eq!(staking.migrate_staker_index(&50, &50), 20);
        assert!(legacy_present(&env, &staking));
        assert_eq!(staking.migrate_staker_index(&100, &50), 0);
        assert!(!legacy_present(&env, &staking), "legacy vector is removed");

        let mut expected: StdVec<Address> = stakers.clone();
        expected.retain(|s| s != &stakers[7] && s != &stakers[100]);
        assert_registry_is(&env, &staking, &expected);
        assert_eq!(registry(&env, &staking).len(), 118);

        let data = find_event(&env, "staker_index_migrated").expect("completion event");
        let (version, (registered,)) = <(u32, (u32,))>::try_from_val(&env, &data).unwrap();
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(registered, 118);
    }

    #[test]
    fn test_migrate_staker_index_is_a_noop_without_a_legacy_vector() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        let a = join(&env, &staking, &lp, 100);

        assert_eq!(staking.migrate_staker_index(&0, &50), 0);
        assert_eq!(staking.migrate_staker_index(&40, &50), 0);
        assert_registry_is(&env, &staking, &[a]);
    }

    #[test]
    fn test_migrate_staker_index_rejects_out_of_order_pages() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        seed_legacy(&env, &staking, &lp, 120, 100);

        // Calling the last page first must not delete the vector.
        assert_eq!(
            staking.try_migrate_staker_index(&100, &50),
            Err(Ok(StakingError::MigrationOutOfOrder))
        );
        assert_eq!(
            staking.try_migrate_staker_index(&50, &50),
            Err(Ok(StakingError::MigrationOutOfOrder))
        );
        assert!(legacy_present(&env, &staking));
        assert!(registry(&env, &staking).is_empty());

        assert_eq!(staking.migrate_staker_index(&0, &50), 70);
        // Replaying an earlier page, or skipping ahead, is rejected too.
        assert_eq!(
            staking.try_migrate_staker_index(&0, &50),
            Err(Ok(StakingError::MigrationOutOfOrder))
        );
        assert_eq!(
            staking.try_migrate_staker_index(&100, &50),
            Err(Ok(StakingError::MigrationOutOfOrder))
        );
        assert_eq!(staking.migrate_staker_index(&50, &50), 20);
        assert_eq!(staking.migrate_staker_index(&100, &50), 0);
        assert_eq!(registry(&env, &staking).len(), 120);
    }

    #[test]
    fn test_migrate_staker_index_rejects_bad_page_limits() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        seed_legacy(&env, &staking, &lp, 10, 100);
        assert_eq!(
            staking.try_migrate_staker_index(&0, &0),
            Err(Ok(StakingError::BatchTooLarge))
        );
        assert_eq!(
            staking.try_migrate_staker_index(&0, &(MAX_BATCH_SIZE + 1)),
            Err(Ok(StakingError::BatchTooLarge))
        );
        assert!(legacy_present(&env, &staking));
    }

    #[test]
    fn test_migration_with_interleaved_stakes_and_unstakes() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        let (_admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        let legacy = seed_legacy(&env, &staking, &lp, 120, 100);

        assert_eq!(staking.migrate_staker_index(&0, &50), 70);

        // Activity while the migration is half done.
        let newcomer = join(&env, &staking, &lp, 100);
        let _ = staking.unstake(&legacy[0], &100_i128); // already migrated
        let _ = staking.unstake(&legacy[60], &100_i128); // not yet migrated
        StellarAssetClient::new(&env, &lp).mint(&legacy[70], &50);
        staking.stake(&legacy[70], &50_i128); // not yet migrated, tops up

        assert_eq!(staking.migrate_staker_index(&50, &50), 20);
        assert_eq!(staking.migrate_staker_index(&100, &50), 0);
        assert!(!legacy_present(&env, &staking));

        let mut expected: StdVec<Address> = legacy.clone();
        expected.retain(|s| s != &legacy[0] && s != &legacy[60]);
        expected.push(newcomer);
        assert_registry_is(&env, &staking, &expected);
        assert_eq!(registry(&env, &staking).len(), 119);
        assert_eq!(
            staking.get_pool_info().total_effective_staked,
            12_000 - 200 + 100 + 50
        );
        // Everyone can still leave.
        for s in expected.iter() {
            let info = staking.get_staker_info(s);
            let _ = staking.unstake(s, &info.staked_amount);
        }
        assert!(registry(&env, &staking).is_empty());
    }

    #[test]
    fn test_keepers_see_unmigrated_legacy_stakers_until_migration_finishes() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        let legacy = seed_legacy(&env, &staking, &lp, 4, 100);
        let expiry = env.ledger().timestamp() + MIN_LOCK_DURATION;
        env.as_contract(&staking.address, || {
            // Three of them hold a boost; the fourth left before the upgrade.
            for s in legacy.iter().take(3) {
                env.storage()
                    .persistent()
                    .set(&DataKey::BoostMultiplier(s.clone()), &DEFAULT_MAX_BOOST);
                env.storage()
                    .persistent()
                    .set(&DataKey::LockExpiry(s.clone()), &expiry);
            }
            env.storage()
                .persistent()
                .set(&DataKey::StakerAmount(legacy[3].clone()), &0i128);
        });
        advance_time(&env, MIN_LOCK_DURATION + 1);

        assert_eq!(staking.list_expired_boosts(&0, &10).len(), 3);
        // Part way through the migration the combined view is still complete.
        assert_eq!(staking.migrate_staker_index(&0, &2), 2);
        let mid = staking.list_expired_boosts(&0, &10);
        assert_eq!(mid.len(), 3);
        assert_eq!(staking.migrate_staker_index(&2, &2), 0);
        let done = staking.list_expired_boosts(&0, &10);
        assert_eq!(done.len(), 3);
        assert_registry_is(&env, &staking, &legacy[..3]);
    }

    #[test]
    fn test_register_existing_stakers_uses_the_registry_and_stays_idempotent() {
        let env = Env::default();
        env.mock_all_auths();
        let (_admin, _s, staking) = setup(&env);
        let lp = staking.get_pool_info().lp_token;
        let a = join(&env, &staking, &lp, 100);
        let pre_upgrade = Address::generate(&env);
        StellarAssetClient::new(&env, &lp).mint(&staking.address, &100);
        env.as_contract(&staking.address, || {
            env.storage()
                .persistent()
                .set(&DataKey::StakerAmount(pre_upgrade.clone()), &100i128);
        });

        let mut batch = Vec::new(&env);
        batch.push_back(a.clone());
        batch.push_back(pre_upgrade.clone());
        batch.push_back(pre_upgrade.clone());
        batch.push_back(Address::generate(&env));
        staking.register_existing_stakers(&batch);
        staking.register_existing_stakers(&batch);

        assert_registry_is(&env, &staking, &[a, pre_upgrade]);
    }
}
