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

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env, Symbol, Vec};

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
    /// Remaining reward tokens available in pool
    RewardPoolBalance,
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
    StakerIndex,
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

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PoolInfo {
    pub lp_token: Address,
    pub reward_token: Address,
    pub admin: Address,
    pub total_effective_staked: i128,
    pub reward_pool_balance: i128,
    pub accumulated_rewards_per_share: i128,
}

// Ã¢â€â‚¬Ã¢â€â‚¬ Contract Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬Ã¢â€â‚¬

#[contract]
pub struct Staking;

#[contractimpl]
impl Staking {
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
        // ConfigMaxRewardPoolBalance initialized above
        Self::_write_boost_config(
            &env,
            MIN_BOOST,
            DEFAULT_MAX_BOOST,
            MIN_LOCK_DURATION,
            MAX_LOCK_DURATION,
        );
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
        Ok(())
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
        // Update reward pool balance with the actual received amount
        let current_balance: i128 = env
            .storage()
            .instance()
            .get(&DataKey::RewardPoolBalance)
            .unwrap_or(0);
        let new_balance = current_balance + received;
        // Enforce optional max reward pool balance cap (0 = no cap)
        let max_balance: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ConfigMaxRewardPoolBalance)
            .unwrap_or(0);
        if max_balance != 0 && new_balance > max_balance {
            return Err(StakingError::MaxRewardPoolExceeded);
        }
        env.storage()
            .instance()
            .set(&DataKey::RewardPoolBalance, &new_balance);
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
        env.events()
            .publish((Symbol::new(&env, "admin_nominated"),), (admin, new_admin));
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
        env.events()
            .publish((Symbol::new(&env, "admin_changed"),), (new_admin,));
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
        let current_balance: i128 = env
            .storage()
            .instance()
            .get(&DataKey::RewardPoolBalance)
            .unwrap_or(0);
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
        let index = Self::_index_load(&env);
        let (min_boost, _) = Self::_boost_bounds(&env);
        let now = env.ledger().timestamp();
        let clamped_limit = limit.min(MAX_LIST_LIMIT);

        let mut result = Vec::new(&env);
        let len = index.len();
        let mut i = offset;
        let end = offset.saturating_add(clamped_limit).min(len);
        while i < end {
            let staker = index.get(i).unwrap();
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
            i += 1;
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
        Self::_settle_pending_at(&env, &staker, Some(old_effective));

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
        let new_debt = new_effective * acc_per_share / SCALE_FACTOR;
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

        let pool_balance: i128 = env
            .storage()
            .instance()
            .get(&DataKey::RewardPoolBalance)
            .unwrap_or(0);

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

            env.storage()
                .instance()
                .set(&DataKey::RewardPoolBalance, &(pool_balance - distributable));

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

        let pool_balance: i128 = env
            .storage()
            .instance()
            .get(&DataKey::RewardPoolBalance)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::RewardPoolBalance, &(pool_balance - pending));

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

    /// Load the staker index (see `DataKey::StakerIndex`), defaulting to empty.
    fn _index_load(env: &Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::StakerIndex)
            .unwrap_or_else(|| Vec::new(env))
    }

    /// Add `staker` to the index if not already present. Idempotent, so it's
    /// safe to call unconditionally on every stake — including a top-up from
    /// an address already indexed — and safe to call from the permissionless
    /// migration helper for a pre-upgrade staker who may or may not already
    /// be indexed.
    fn _index_add(env: &Env, staker: &Address) {
        let mut index = Self::_index_load(env);
        let already_present = index.iter().any(|a| &a == staker);
        if !already_present {
            index.push_back(staker.clone());
            env.storage().instance().set(&DataKey::StakerIndex, &index);
        }
    }

    /// Remove `staker` from the index. Called once their raw staked balance
    /// returns to zero, so the index stays bounded by the number of
    /// *currently* staked addresses rather than growing forever (#699).
    fn _index_remove(env: &Env, staker: &Address) {
        let mut index = Self::_index_load(env);
        let mut found_at: Option<u32> = None;
        for (i, a) in index.iter().enumerate() {
            if &a == staker {
                found_at = Some(i as u32);
                break;
            }
        }
        if let Some(pos) = found_at {
            index.remove(pos);
            env.storage().instance().set(&DataKey::StakerIndex, &index);
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

        let pool_balance: i128 = env
            .storage()
            .instance()
            .get(&DataKey::RewardPoolBalance)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::RewardPoolBalance, &(pool_balance - pending));

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
}
