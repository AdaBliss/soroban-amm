#![no_std]

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, symbol_short, Address,
    Env, Vec,
};

#[contractclient(name = "AmmPoolOracleClient")]
pub trait AmmPoolOracle {
    fn get_price_cumulative(env: Env) -> (i128, i128, u64);
}

#[contractclient(name = "ClPoolOracleClient")]
pub trait ClPoolOracle {
    fn get_tick_cumulative(env: Env) -> (i64, u64);
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TwapError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    ZeroWindow = 3,
    InsufficientHistory = 4,
    NoSnapshotFound = 5,
    ElapsedZero = 6,
    InvalidSpotPrice = 7,
    InvalidTwapPrice = 8,
    InvalidDeviationBps = 9,
    NegativeCollateral = 10,
    PriceManipulated = 11,
    InvalidRetentionPolicy = 12,
    Unauthorized = 13,
}

#[contracttype]
pub enum DataKey {
    Keeper,
    Snapshot(Address, u64),
    TrackedPoolsPersistent,
    /// Sorted (ascending, deduplicated) ledger timestamps at which a snapshot
    /// was saved for this pool. Lets `get_twap_*` binary-search for the most
    /// recent snapshot at or before an arbitrary `then_ts` instead of
    /// requiring an exact-timestamp hit (issue #469).
    SnapshotTimestamps(Address),
    RetentionPolicy,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceSnapshot {
    pub cum_a: i128,
    pub cum_b: i128,
    pub pool_ts: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceValidation {
    pub spot_price: i128,
    pub twap_price: i128,
    pub deviation_bps: i128,
    pub max_deviation_bps: i128,
    pub is_deviation: bool,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct RetentionPolicy {
    /// Snapshots older than this many seconds are eligible for pruning.
    /// 0 disables age-based pruning.
    pub max_age_seconds: u64,
    /// Hard cap on snapshots retained per pool. 0 disables count-based pruning.
    pub max_snapshots_per_pool: u32,
}

#[contract]
pub struct TwapConsumer;

#[contractimpl]
impl TwapConsumer {
    pub const SNAPSHOT_TTL_LEDGERS: u32 = 120_960;
    pub const BPS_DENOMINATOR: i128 = 10_000;
    pub const PRICE_SCALE: i128 = 1_000_000;
    /// Longest supported TWAP window (24 hours = 86,400 seconds).
    /// Retention policies with max_age_seconds shorter than this are rejected
    /// to avoid deleting data the oracle still needs.
    pub const LONGEST_TWAP_WINDOW: u64 = 86_400;
    /// Default max age when retention policy is unset (7 days = 604,800 seconds).
    pub const DEFAULT_MAX_AGE_SECONDS: u64 = 604_800;
    /// Default max snapshots per pool when retention policy is unset (0 = count cap disabled).
    pub const DEFAULT_MAX_SNAPSHOTS_PER_POOL: u32 = 0;
    /// Maximum number of eligible snapshots opportunistically pruned during save_snapshot.
    pub const AMORTIZED_PRUNE_LIMIT: u32 = 2;

    /// Instance-storage TTL: below this many remaining ledgers, `extend_ttl`
    /// renews the entry; each renewal bumps it back up to `INSTANCE_TTL_BUMP_TO`.
    ///
    /// The instance entry holds the keeper address and retention policy —
    /// the state every entrypoint needs just to authorize or read. Oracle
    /// consumers like this one are read by other protocols sporadically
    /// rather than driven by steady user traffic (see #910), so the
    /// threshold can't assume frequent calls will keep it alive on their
    /// own. `172_800` ledgers (~10 days at 5s/ledger) is the same floor
    /// `contracts/amm` uses for its own instance entry, chosen so a renewal
    /// still has slack before the ~30-day (`518_400`-ledger) archival
    /// horizon docs generally assume for a "recently touched" contract.
    pub const INSTANCE_TTL_THRESHOLD: u32 = 172_800;
    pub const INSTANCE_TTL_BUMP_TO: u32 = 518_400;

    /// Extends the contract's **instance** storage TTL (keeper, retention
    /// policy). Safe to call on every entrypoint — `extend_ttl` is a no-op
    /// until the entry's remaining TTL drops below `INSTANCE_TTL_THRESHOLD`.
    fn extend_instance_ttl(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(Self::INSTANCE_TTL_THRESHOLD, Self::INSTANCE_TTL_BUMP_TO);
    }

    pub fn initialize(env: Env, keeper: Address) -> Result<(), TwapError> {
        Self::extend_instance_ttl(&env);
        if env.storage().instance().has(&DataKey::Keeper) {
            return Err(TwapError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Keeper, &keeper);
        Ok(())
    }

    pub fn get_keeper(env: Env) -> Result<Address, TwapError> {
        Self::extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Keeper)
            .ok_or(TwapError::NotInitialized)
    }

    fn require_keeper(env: &Env) -> Result<(), TwapError> {
        Self::get_keeper(env.clone())?.require_auth();
        Ok(())
    }

    /// Sets the snapshot retention policy. Caller must be the keeper/admin.
    /// Rejects policies with `0 < max_age_seconds < LONGEST_TWAP_WINDOW`.
    pub fn set_retention_policy(
        env: Env,
        admin: Address,
        policy: RetentionPolicy,
    ) -> Result<(), TwapError> {
        Self::extend_instance_ttl(&env);
        let keeper = Self::get_keeper(env.clone())?;
        if admin != keeper {
            return Err(TwapError::Unauthorized);
        }
        admin.require_auth();

        if policy.max_age_seconds > 0 && policy.max_age_seconds < Self::LONGEST_TWAP_WINDOW {
            return Err(TwapError::InvalidRetentionPolicy);
        }

        env.storage()
            .instance()
            .set(&DataKey::RetentionPolicy, &policy);
        Ok(())
    }

    /// Returns the active retention policy, or a default policy with
    /// `max_age_seconds = 604_800` (7 days) and `max_snapshots_per_pool = 0` (unlimited).
    pub fn get_retention_policy(env: Env) -> RetentionPolicy {
        Self::extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::RetentionPolicy)
            .unwrap_or(RetentionPolicy {
                max_age_seconds: Self::DEFAULT_MAX_AGE_SECONDS,
                max_snapshots_per_pool: Self::DEFAULT_MAX_SNAPSHOTS_PER_POOL,
            })
    }

    /// Returns the number of snapshots tracked in the index for `pool`.
    pub fn get_snapshot_count(env: Env, pool: Address) -> u32 {
        Self::extend_instance_ttl(&env);
        let timestamps: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::SnapshotTimestamps(pool))
            .unwrap_or_else(|| Vec::new(&env));
        timestamps.len()
    }

    /// Returns a paginated slice of snapshot timestamps for `pool`.
    pub fn list_snapshot_timestamps(env: Env, pool: Address, offset: u32, limit: u32) -> Vec<u64> {
        Self::extend_instance_ttl(&env);
        let timestamps: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::SnapshotTimestamps(pool))
            .unwrap_or_else(|| Vec::new(&env));
        let len = timestamps.len();
        let mut result = Vec::new(&env);
        if offset >= len || limit == 0 {
            return result;
        }
        let end = (offset + limit).min(len);
        for i in offset..end {
            result.push_back(timestamps.get(i).unwrap());
        }
        result
    }

    /// Returns snapshots with timestamps in `[from_ts, to_ts]`, up to `limit` entries.
    pub fn get_snapshots(
        env: Env,
        pool: Address,
        from_ts: u64,
        to_ts: u64,
        limit: u32,
    ) -> Vec<(u64, PriceSnapshot)> {
        Self::extend_instance_ttl(&env);
        let timestamps: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::SnapshotTimestamps(pool.clone()))
            .unwrap_or_else(|| Vec::new(&env));
        let mut result = Vec::new(&env);
        let max_items = if limit == 0 { u32::MAX } else { limit };
        for i in 0..timestamps.len() {
            if result.len() >= max_items {
                break;
            }
            let ts = timestamps.get(i).unwrap();
            if ts >= from_ts && ts <= to_ts {
                if let Some(snap) = env
                    .storage()
                    .persistent()
                    .get(&DataKey::Snapshot(pool.clone(), ts))
                {
                    result.push_back((ts, snap));
                }
            }
        }
        result
    }

    pub fn save_snapshot(env: Env, pool: Address) -> Result<(), TwapError> {
        Self::extend_instance_ttl(&env);
        Self::require_keeper(&env)?;
        let (cum_a, cum_b, pool_ts) = AmmPoolOracleClient::new(&env, &pool).get_price_cumulative();
        let ledger_ts = env.ledger().timestamp();
        let snapshot = PriceSnapshot {
            cum_a,
            cum_b,
            pool_ts,
        };
        let key = DataKey::Snapshot(pool.clone(), ledger_ts);
        env.storage().persistent().set(&key, &snapshot);
        env.storage().persistent().extend_ttl(
            &key,
            Self::SNAPSHOT_TTL_LEDGERS / 2,
            Self::SNAPSHOT_TTL_LEDGERS,
        );
        Self::record_snapshot_timestamp(&env, &pool, ledger_ts);

        let mut tracked: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::TrackedPoolsPersistent)
            .unwrap_or_else(|| Vec::new(&env));
        let mut already_tracked = false;
        for i in 0..tracked.len() {
            if tracked.get(i).unwrap() == pool {
                already_tracked = true;
                break;
            }
        }
        if !already_tracked {
            tracked.push_back(pool.clone());
            env.storage()
                .persistent()
                .set(&DataKey::TrackedPoolsPersistent, &tracked);
            env.storage().persistent().extend_ttl(
                &DataKey::TrackedPoolsPersistent,
                Self::SNAPSHOT_TTL_LEDGERS / 2,
                Self::SNAPSHOT_TTL_LEDGERS,
            );
        }

        // Opportunistic bounded amortised pruning
        let _ = Self::prune_snapshots_internal(&env, &pool, Self::AMORTIZED_PRUNE_LIMIT);
        Ok(())
    }

    /// Deletes a price snapshot from persistent storage.
    /// Returns `TwapError::NoSnapshotFound` and emits no event if the snapshot does not exist.
    pub fn delete_snapshot(env: Env, pool: Address, ledger_ts: u64) -> Result<(), TwapError> {
        Self::extend_instance_ttl(&env);
        Self::require_keeper(&env)?;
        let key = DataKey::Snapshot(pool.clone(), ledger_ts);
        if !env.storage().persistent().has(&key) {
            return Err(TwapError::NoSnapshotFound);
        }
        env.storage().persistent().remove(&key);
        Self::remove_snapshot_timestamp(&env, &pool, ledger_ts);
        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (symbol_short!("snap_del"), pool),
            ledger_ts
        );
        Ok(())
    }

    /// Internal helper that implements bounded pruning for a single pool.
    fn prune_snapshots_internal(env: &Env, pool: &Address, max_to_remove: u32) -> u32 {
        if max_to_remove == 0 {
            return 0;
        }
        let policy = Self::get_retention_policy(env.clone());
        let current_ts = env.ledger().timestamp();
        let key = DataKey::SnapshotTimestamps(pool.clone());
        let timestamps: Vec<u64> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(env));
        let total_count = timestamps.len();
        if total_count == 0 {
            return 0;
        }

        let mut remove_count = 0u32;
        let mut remaining_timestamps = Vec::new(env);

        for i in 0..total_count {
            let ts = timestamps.get(i).unwrap();
            let remaining_count = total_count - remove_count;

            let age_eligible = policy.max_age_seconds > 0
                && current_ts >= ts.saturating_add(policy.max_age_seconds);
            let count_eligible = policy.max_snapshots_per_pool > 0
                && remaining_count > policy.max_snapshots_per_pool;

            if (age_eligible || count_eligible) && remove_count < max_to_remove {
                env.storage()
                    .persistent()
                    .remove(&DataKey::Snapshot(pool.clone(), ts));
                remove_count += 1;
            } else {
                remaining_timestamps.push_back(ts);
            }
        }

        if remove_count > 0 {
            env.storage().persistent().set(&key, &remaining_timestamps);
            env.storage().persistent().extend_ttl(
                &key,
                Self::SNAPSHOT_TTL_LEDGERS / 2,
                Self::SNAPSHOT_TTL_LEDGERS,
            );
            let oldest_remaining_ts = remaining_timestamps.first().unwrap_or(0);
            soroban_amm_sdk::emit_versioned_event!(
                env,
                (symbol_short!("pruned"), pool.clone()),
                (remove_count, oldest_remaining_ts)
            );
        }

        remove_count
    }

    /// Permissionless bounded pruning for a pool according to the active retention policy.
    pub fn prune_snapshots(env: Env, pool: Address, max_to_remove: u32) -> u32 {
        Self::extend_instance_ttl(&env);
        Self::prune_snapshots_internal(&env, &pool, max_to_remove)
    }

    /// Permissionless sweep across all tracked pools, removing up to `max_to_remove_per_pool`
    /// eligible snapshots per pool. Fault-isolated so one pool cannot abort the sweep.
    pub fn prune_all(env: Env, max_to_remove_per_pool: u32) -> u32 {
        Self::extend_instance_ttl(&env);
        let tracked: Vec<Address> = Self::get_tracked_pools(env.clone());
        let mut total_removed = 0u32;
        for i in 0..tracked.len() {
            let pool = tracked.get(i).unwrap();
            let removed = Self::prune_snapshots_internal(&env, &pool, max_to_remove_per_pool);
            total_removed = total_removed.saturating_add(removed);
        }
        total_removed
    }

    /// Record `ts` in the pool's sorted snapshot-timestamp index. Ledger
    /// timestamps are non-decreasing across calls, so appending keeps the
    /// index sorted; skip if `ts` is already the most recent entry so a
    /// keeper re-saving within the same ledger doesn't create a duplicate.
    fn record_snapshot_timestamp(env: &Env, pool: &Address, ts: u64) {
        let key = DataKey::SnapshotTimestamps(pool.clone());
        let mut timestamps: Vec<u64> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(env));
        if timestamps.last() != Some(ts) {
            timestamps.push_back(ts);
        }
        env.storage().persistent().set(&key, &timestamps);
        env.storage().persistent().extend_ttl(
            &key,
            Self::SNAPSHOT_TTL_LEDGERS / 2,
            Self::SNAPSHOT_TTL_LEDGERS,
        );
    }

    fn remove_snapshot_timestamp(env: &Env, pool: &Address, ts: u64) {
        let key = DataKey::SnapshotTimestamps(pool.clone());
        let timestamps: Vec<u64> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(env));
        let mut updated: Vec<u64> = Vec::new(env);
        for i in 0..timestamps.len() {
            let t = timestamps.get(i).unwrap();
            if t != ts {
                updated.push_back(t);
            }
        }
        env.storage().persistent().set(&key, &updated);
    }

    /// Binary-search the pool's snapshot-timestamp index for the most recent
    /// entry at or before `then_ts` (the "floor"). Returns `None` if no
    /// snapshot that old exists.
    fn floor_snapshot_ts(env: &Env, pool: &Address, then_ts: u64) -> Option<u64> {
        let timestamps: Vec<u64> = env
            .storage()
            .persistent()
            .get(&DataKey::SnapshotTimestamps(pool.clone()))
            .unwrap_or_else(|| Vec::new(env));
        let mut lo: u32 = 0;
        let mut hi: u32 = timestamps.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if timestamps.get(mid).unwrap() <= then_ts {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            None
        } else {
            Some(timestamps.get(lo - 1).unwrap())
        }
    }

    pub fn get_twap_price(env: Env, pool: Address, window_seconds: u64) -> Result<i128, TwapError> {
        Self::extend_instance_ttl(&env);
        if window_seconds == 0 {
            return Err(TwapError::ZeroWindow);
        }
        let (cum_a_now, _cum_b_now, pool_ts_now) =
            AmmPoolOracleClient::new(&env, &pool).get_price_cumulative();
        let ledger_ts_now = env.ledger().timestamp();
        if ledger_ts_now < window_seconds {
            return Err(TwapError::InsufficientHistory);
        }
        let then_ts = ledger_ts_now - window_seconds;
        let floor_ts =
            Self::floor_snapshot_ts(&env, &pool, then_ts).ok_or(TwapError::InsufficientHistory)?;
        let snapshot: PriceSnapshot = env
            .storage()
            .persistent()
            .get(&DataKey::Snapshot(pool.clone(), floor_ts))
            .ok_or(TwapError::NoSnapshotFound)?;

        let delta_a = (cum_a_now as u128).wrapping_sub(snapshot.cum_a as u128) as i128;
        let elapsed = (pool_ts_now - snapshot.pool_ts) as i128;
        if elapsed <= 0 {
            return Err(TwapError::ElapsedZero);
        }
        Ok(delta_a / elapsed)
    }

    pub fn validate_price(
        spot_price: i128,
        twap_price: i128,
        max_deviation_bps: i128,
    ) -> Result<PriceValidation, TwapError> {
        if spot_price <= 0 {
            return Err(TwapError::InvalidSpotPrice);
        }
        if twap_price <= 0 {
            return Err(TwapError::InvalidTwapPrice);
        }
        if !(0..=Self::BPS_DENOMINATOR).contains(&max_deviation_bps) {
            return Err(TwapError::InvalidDeviationBps);
        }
        let price_delta = if spot_price >= twap_price {
            spot_price - twap_price
        } else {
            twap_price - spot_price
        };
        let deviation_bps = price_delta * Self::BPS_DENOMINATOR / twap_price;
        Ok(PriceValidation {
            spot_price,
            twap_price,
            deviation_bps,
            max_deviation_bps,
            is_deviation: deviation_bps > max_deviation_bps,
        })
    }

    pub fn validate_price_against_twap(
        env: Env,
        pool: Address,
        window_seconds: u64,
        spot_price: i128,
        max_deviation_bps: i128,
    ) -> Result<PriceValidation, TwapError> {
        Self::extend_instance_ttl(&env);
        let twap_price = Self::get_twap_price(env, pool, window_seconds)?;
        Self::validate_price(spot_price, twap_price, max_deviation_bps)
    }

    pub fn assert_lending_price_safe(
        env: Env,
        pool: Address,
        window_seconds: u64,
        spot_price: i128,
        max_deviation_bps: i128,
        collateral_amount: i128,
    ) -> Result<i128, TwapError> {
        Self::extend_instance_ttl(&env);
        if collateral_amount < 0 {
            return Err(TwapError::NegativeCollateral);
        }
        let validation = Self::validate_price_against_twap(
            env,
            pool,
            window_seconds,
            spot_price,
            max_deviation_bps,
        )?;
        if validation.is_deviation {
            return Err(TwapError::PriceManipulated);
        }
        Ok(collateral_amount * validation.spot_price / Self::PRICE_SCALE)
    }

    pub fn get_twap_both(
        env: Env,
        pool: Address,
        window_seconds: u64,
    ) -> Result<(i128, i128), TwapError> {
        Self::extend_instance_ttl(&env);
        if window_seconds == 0 {
            return Err(TwapError::ZeroWindow);
        }
        let (cum_a_now, cum_b_now, pool_ts_now) =
            AmmPoolOracleClient::new(&env, &pool).get_price_cumulative();
        let ledger_ts_now = env.ledger().timestamp();
        if ledger_ts_now < window_seconds {
            return Err(TwapError::InsufficientHistory);
        }
        let then_ts = ledger_ts_now - window_seconds;
        let floor_ts =
            Self::floor_snapshot_ts(&env, &pool, then_ts).ok_or(TwapError::InsufficientHistory)?;
        let snapshot: PriceSnapshot = env
            .storage()
            .persistent()
            .get(&DataKey::Snapshot(pool.clone(), floor_ts))
            .ok_or(TwapError::NoSnapshotFound)?;

        let delta_a = (cum_a_now as u128).wrapping_sub(snapshot.cum_a as u128) as i128;
        let delta_b = (cum_b_now as u128).wrapping_sub(snapshot.cum_b as u128) as i128;
        let elapsed = (pool_ts_now - snapshot.pool_ts) as i128;
        if elapsed <= 0 {
            return Err(TwapError::ElapsedZero);
        }
        Ok((delta_a / elapsed, delta_b / elapsed))
    }

    pub fn get_tracked_pools(env: Env) -> Vec<Address> {
        Self::extend_instance_ttl(&env);
        env.storage()
            .persistent()
            .get(&DataKey::TrackedPoolsPersistent)
            .unwrap_or_else(|| Vec::new(&env))
    }

    pub fn get_twap_all(env: Env, window_seconds: u64) -> Result<Vec<(Address, i128)>, TwapError> {
        Self::extend_instance_ttl(&env);
        let tracked: Vec<Address> = Self::get_tracked_pools(env.clone());
        let mut results: Vec<(Address, i128)> = Vec::new(&env);
        for i in 0..tracked.len() {
            let pool = tracked.get(i).unwrap();
            let twap = Self::get_twap_price(env.clone(), pool.clone(), window_seconds)?;
            results.push_back((pool, twap));
        }
        Ok(results)
    }

    pub fn get_cl_twap(env: Env, pool: Address, window_seconds: u64) -> Result<i64, TwapError> {
        Self::extend_instance_ttl(&env);
        if window_seconds == 0 {
            return Err(TwapError::ZeroWindow);
        }
        let (cum_now, last_ts_now) = ClPoolOracleClient::new(&env, &pool).get_tick_cumulative();
        let ledger_ts_now = env.ledger().timestamp();
        if ledger_ts_now < window_seconds {
            return Err(TwapError::InsufficientHistory);
        }
        let then_ts = ledger_ts_now - window_seconds;
        let floor_ts =
            Self::floor_snapshot_ts(&env, &pool, then_ts).ok_or(TwapError::InsufficientHistory)?;
        let snapshot: PriceSnapshot = env
            .storage()
            .persistent()
            .get(&DataKey::Snapshot(pool.clone(), floor_ts))
            .ok_or(TwapError::NoSnapshotFound)?;

        let cum_then = snapshot.cum_a as i64;
        let elapsed_pool = (last_ts_now - snapshot.pool_ts) as i64;
        if elapsed_pool <= 0 {
            return Err(TwapError::ElapsedZero);
        }
        Ok((cum_now - cum_then) / elapsed_pool)
    }

    pub fn save_cl_snapshot(env: Env, pool: Address) -> Result<(), TwapError> {
        Self::extend_instance_ttl(&env);
        Self::require_keeper(&env)?;
        let (tick_cum, pool_ts) = ClPoolOracleClient::new(&env, &pool).get_tick_cumulative();
        let ledger_ts = env.ledger().timestamp();
        let snapshot = PriceSnapshot {
            cum_a: tick_cum as i128,
            cum_b: 0,
            pool_ts,
        };
        let key = DataKey::Snapshot(pool.clone(), ledger_ts);
        env.storage().persistent().set(&key, &snapshot);
        env.storage().persistent().extend_ttl(
            &key,
            Self::SNAPSHOT_TTL_LEDGERS / 2,
            Self::SNAPSHOT_TTL_LEDGERS,
        );
        Self::record_snapshot_timestamp(&env, &pool, ledger_ts);

        let mut tracked: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::TrackedPoolsPersistent)
            .unwrap_or_else(|| Vec::new(&env));
        let mut already_tracked = false;
        for i in 0..tracked.len() {
            if tracked.get(i).unwrap() == pool {
                already_tracked = true;
                break;
            }
        }
        if !already_tracked {
            tracked.push_back(pool.clone());
            env.storage()
                .persistent()
                .set(&DataKey::TrackedPoolsPersistent, &tracked);
            env.storage().persistent().extend_ttl(
                &DataKey::TrackedPoolsPersistent,
                Self::SNAPSHOT_TTL_LEDGERS / 2,
                Self::SNAPSHOT_TTL_LEDGERS,
            );
        }

        // Opportunistic bounded amortised pruning
        let _ = Self::prune_snapshots_internal(&env, &pool, Self::AMORTIZED_PRUNE_LIMIT);
        Ok(())
    }
}

/// Minimal mock CL pool used by tests only. Satisfies the `ClPoolOracle`
/// interface (`get_tick_cumulative`) without requiring the full CL contract.
#[cfg(test)]
mod mock_cl_pool {
    use soroban_sdk::{contract, contractimpl, Env};

    #[contract]
    pub struct MockClPool;

    #[contractimpl]
    impl MockClPool {
        pub fn get_tick_cumulative(_env: Env) -> (i64, u64) {
            (1_000_i64, 10_000_u64)
        }
    }
}

#[cfg(test)]
use mock_cl_pool::MockClPool;

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    use amm::{AmmPool, AmmPoolClient};
    use soroban_sdk::{
        testutils::{storage::Instance as _, Address as _, Events as _, Ledger},
        token::{StellarAssetClient, TokenClient as StellarTokenClient},
        Address, Env, IntoVal,
    };
    use token::LpToken;

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

    /// Deploys an `AmmPool`, seeds it with `reserve_a`/`reserve_b` liquidity,
    /// deploys a `TwapConsumer`, and saves an initial snapshot. Returns the
    /// pool address and a client for the consumer.
    fn setup_pool_and_consumer<'a>(
        env: &'a Env,
        admin: &Address,
        reserve_a: i128,
        reserve_b: i128,
    ) -> (Address, TwapConsumerClient<'a>) {
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwapConsumer);

        token::LpTokenClient::new(env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(env, "AMM LP Token"),
            &soroban_sdk::String::from_str(env, "ALP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(env, admin);
        let (tb, tb_sac) = create_sac(env, admin);

        let amm = AmmPoolClient::new(env, &amm_addr);
        amm.initialize(
            admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            admin,
            &0_i128,
        );

        let provider = Address::generate(env);
        ta_sac.mint(&provider, &reserve_a);
        tb_sac.mint(&provider, &reserve_b);
        amm.add_liquidity(
            &provider,
            &reserve_a,
            &reserve_b,
            &0_i128,
            &(env.ledger().timestamp() + 10_000),
        );

        let consumer = TwapConsumerClient::new(env, &consumer_addr);
        consumer.initialize(admin);
        consumer.save_snapshot(&amm_addr);

        (amm_addr, consumer)
    }

    #[test]
    fn test_get_twap_price_diverges_from_spot_after_large_trade() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwapConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &2_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);
        amm.add_liquidity(
            &provider,
            &2_000_000_i128,
            &2_000_000_i128,
            &0_i128,
            &10_000_u64,
        );

        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);
        consumer.save_snapshot(&amm_addr);

        env.ledger().set_timestamp(10_060);
        let whale = Address::generate(&env);
        ta_sac.mint(&whale, &1_000_000_i128);
        amm.swap(&whale, &ta.address, &1_000_000_i128, &0_i128, &10_060_u64);

        let twap = consumer.get_twap_price(&amm_addr, &60_u64);
        let (spot_a, _spot_b) = amm.price_ratio();

        assert_eq!(twap, 1_000_000);
        assert!(twap > spot_a);
        assert_ne!(twap, spot_a);
    }

    #[test]
    fn test_validate_price_against_twap_flags_large_deviation() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwapConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &2_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);
        amm.add_liquidity(
            &provider,
            &2_000_000_i128,
            &2_000_000_i128,
            &0_i128,
            &10_000_u64,
        );

        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);
        consumer.save_snapshot(&amm_addr);

        env.ledger().set_timestamp(10_060);
        let whale = Address::generate(&env);
        ta_sac.mint(&whale, &1_000_000_i128);
        amm.swap(&whale, &ta.address, &1_000_000_i128, &0_i128, &10_060_u64);

        let (spot_a, _spot_b) = amm.price_ratio();
        let validation =
            consumer.validate_price_against_twap(&amm_addr, &60_u64, &spot_a, &500_i128);

        assert_eq!(validation.twap_price, 1_000_000);
        assert_eq!(validation.max_deviation_bps, 500);
        assert!(validation.deviation_bps > 500);
        assert!(validation.is_deviation);
    }

    #[test]
    fn test_lending_helper_accepts_safe_price_and_rejects_manipulated_price() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwapConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &2_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);
        amm.add_liquidity(
            &provider,
            &2_000_000_i128,
            &2_000_000_i128,
            &0_i128,
            &10_000_u64,
        );

        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);
        consumer.save_snapshot(&amm_addr);

        env.ledger().set_timestamp(10_060);
        let trader = Address::generate(&env);
        ta_sac.mint(&trader, &1_000_i128);
        amm.swap(&trader, &ta.address, &1_000_i128, &0_i128, &10_060_u64);

        let (safe_spot, _spot_b) = amm.price_ratio();
        let collateral_value = consumer.assert_lending_price_safe(
            &amm_addr,
            &60_u64,
            &safe_spot,
            &500_i128,
            &3_000_000_i128,
        );
        assert!(collateral_value > 0);

        let result = consumer.try_assert_lending_price_safe(
            &amm_addr,
            &60_u64,
            &600_000_i128,
            &500_i128,
            &3_000_000_i128,
        );
        assert_eq!(result, Err(Ok(TwapError::PriceManipulated)));
    }

    #[test]
    fn test_get_twap_both() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwapConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &2_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);
        amm.add_liquidity(
            &provider,
            &2_000_000_i128,
            &2_000_000_i128,
            &0_i128,
            &10_000_u64,
        );

        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);
        consumer.save_snapshot(&amm_addr);

        env.ledger().set_timestamp(10_060);
        let whale = Address::generate(&env);
        ta_sac.mint(&whale, &1_000_i128);
        amm.swap(&whale, &ta.address, &1_000_i128, &0_i128, &10_060_u64);

        let (twap_a_to_b, twap_b_to_a) = consumer.get_twap_both(&amm_addr, &60_u64);
        assert_eq!(twap_a_to_b, 1_000_000);
        assert_eq!(twap_b_to_a, 1_000_000);
    }

    #[test]
    fn test_get_twap_both_with_imbalance() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwapConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &2_000_000_i128);
        tb_sac.mint(&provider, &4_000_000_i128);
        amm.add_liquidity(
            &provider,
            &2_000_000_i128,
            &4_000_000_i128,
            &0_i128,
            &10_000_u64,
        );

        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);
        consumer.save_snapshot(&amm_addr);

        env.ledger().set_timestamp(10_060);
        let whale = Address::generate(&env);
        ta_sac.mint(&whale, &1_000_i128);
        amm.swap(&whale, &ta.address, &1_000_i128, &0_i128, &10_060_u64);

        let (twap_a_to_b, twap_b_to_a) = consumer.get_twap_both(&amm_addr, &60_u64);
        assert_eq!(twap_a_to_b, 2_000_000);
        assert_eq!(twap_b_to_a, 500_000);
    }

    #[test]
    fn test_get_tracked_pools_and_twap_all() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);

        let amm_addr1 = env.register_contract(None, AmmPool);
        let lp_addr1 = env.register_contract(None, LpToken);
        token::LpTokenClient::new(&env, &lp_addr1).initialize(
            &amm_addr1,
            &soroban_sdk::String::from_str(&env, "LP1"),
            &soroban_sdk::String::from_str(&env, "LP1"),
            &7u32,
        );
        let (ta1, ta1_sac) = create_sac(&env, &admin);
        let (tb1, tb1_sac) = create_sac(&env, &admin);
        let amm1 = AmmPoolClient::new(&env, &amm_addr1);
        amm1.initialize(
            &admin,
            &ta1.address,
            &tb1.address,
            &lp_addr1,
            &30_i128,
            &admin,
            &0_i128,
        );
        let p1 = Address::generate(&env);
        ta1_sac.mint(&p1, &2_000_000_i128);
        tb1_sac.mint(&p1, &2_000_000_i128);
        amm1.add_liquidity(&p1, &2_000_000_i128, &2_000_000_i128, &0_i128, &10_000_u64);

        let amm_addr2 = env.register_contract(None, AmmPool);
        let lp_addr2 = env.register_contract(None, LpToken);
        token::LpTokenClient::new(&env, &lp_addr2).initialize(
            &amm_addr2,
            &soroban_sdk::String::from_str(&env, "LP2"),
            &soroban_sdk::String::from_str(&env, "LP2"),
            &7u32,
        );
        let (ta2, ta2_sac) = create_sac(&env, &admin);
        let (tb2, tb2_sac) = create_sac(&env, &admin);
        let amm2 = AmmPoolClient::new(&env, &amm_addr2);
        amm2.initialize(
            &admin,
            &ta2.address,
            &tb2.address,
            &lp_addr2,
            &30_i128,
            &admin,
            &0_i128,
        );
        let p2 = Address::generate(&env);
        ta2_sac.mint(&p2, &2_000_000_i128);
        tb2_sac.mint(&p2, &4_000_000_i128);
        amm2.add_liquidity(&p2, &2_000_000_i128, &4_000_000_i128, &0_i128, &10_000_u64);

        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);
        consumer.save_snapshot(&amm_addr1);
        consumer.save_snapshot(&amm_addr2);

        let tracked = consumer.get_tracked_pools();
        assert_eq!(tracked.len(), 2);
        assert!(tracked.contains(&amm_addr1));
        assert!(tracked.contains(&amm_addr2));

        consumer.save_snapshot(&amm_addr1);
        assert_eq!(consumer.get_tracked_pools().len(), 2);

        env.ledger().set_timestamp(10_060);
        let whale1 = Address::generate(&env);
        ta1_sac.mint(&whale1, &1_000_i128);
        amm1.swap(&whale1, &ta1.address, &1_000_i128, &0_i128, &10_060_u64);
        let whale2 = Address::generate(&env);
        ta2_sac.mint(&whale2, &1_000_i128);
        amm2.swap(&whale2, &ta2.address, &1_000_i128, &0_i128, &10_060_u64);

        let all_twaps = consumer.get_twap_all(&60_u64);
        assert_eq!(all_twaps.len(), 2);

        let twap1 = consumer.get_twap_price(&amm_addr1, &60_u64);
        assert_eq!(twap1, 1_000_000);
        let twap2 = consumer.get_twap_price(&amm_addr2, &60_u64);
        assert_eq!(twap2, 2_000_000);

        for i in 0..all_twaps.len() {
            let (pool, twap) = all_twaps.get(i).unwrap();
            if pool == amm_addr1 {
                assert_eq!(twap, twap1);
            } else {
                assert_eq!(twap, twap2);
            }
        }
    }

    #[test]
    fn test_delete_snapshot() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwapConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &2_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);
        amm.add_liquidity(
            &provider,
            &2_000_000_i128,
            &2_000_000_i128,
            &0_i128,
            &10_000_u64,
        );

        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);
        consumer.save_snapshot(&amm_addr);

        env.ledger().set_timestamp(10_060);
        let whale = Address::generate(&env);
        ta_sac.mint(&whale, &1_000_i128);
        amm.swap(&whale, &ta.address, &1_000_i128, &0_i128, &10_060_u64);
        assert_eq!(consumer.get_twap_price(&amm_addr, &60_u64), 1_000_000);

        consumer.delete_snapshot(&amm_addr, &10_000);

        // The only snapshot at/before then_ts=10_000 was removed from the
        // timestamp index along with the snapshot itself, so there is no
        // floor entry left — this is InsufficientHistory.
        let result = consumer.try_get_twap_price(&amm_addr, &60_u64);
        assert_eq!(result, Err(Ok(TwapError::InsufficientHistory)));
    }

    #[test]
    fn test_get_twap_price_with_arbitrary_window_uses_floor_snapshot() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwapConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP Token"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);

        let amm = AmmPoolClient::new(&env, &amm_addr);
        amm.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &2_000_000_i128);
        tb_sac.mint(&provider, &2_000_000_i128);
        amm.add_liquidity(
            &provider,
            &2_000_000_i128,
            &2_000_000_i128,
            &0_i128,
            &10_000_u64,
        );

        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);
        consumer.save_snapshot(&amm_addr);

        env.ledger().set_timestamp(10_030);
        let whale = Address::generate(&env);
        ta_sac.mint(&whale, &2_000_000_i128);
        amm.swap(&whale, &ta.address, &1_000_000_i128, &0_i128, &10_030_u64);
        consumer.save_snapshot(&amm_addr);

        env.ledger().set_timestamp(10_075);
        amm.swap(&whale, &ta.address, &1_000_000_i128, &0_i128, &10_075_u64);

        let exact_hit = consumer.get_twap_price(&amm_addr, &45_u64);
        assert!(exact_hit > 0);

        let floor_hit = consumer.get_twap_price(&amm_addr, &50_u64);
        assert!(floor_hit > 0);
    }

    #[test]
    fn test_zero_window_returns_error() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let pool = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);

        assert_eq!(
            consumer.try_get_twap_price(&pool, &0_u64),
            Err(Ok(TwapError::ZeroWindow))
        );
        assert_eq!(
            consumer.try_get_twap_both(&pool, &0_u64),
            Err(Ok(TwapError::ZeroWindow))
        );
        assert_eq!(
            consumer.try_get_cl_twap(&pool, &0_u64),
            Err(Ok(TwapError::ZeroWindow))
        );
    }

    #[test]
    fn test_save_snapshot_requires_keeper_auth() {
        let env = Env::default();
        env.ledger().set_timestamp(10_000);

        let keeper = Address::generate(&env);
        let pool = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);

        assert!(consumer.try_save_snapshot(&pool).is_err());
        assert!(consumer.try_delete_snapshot(&pool, &10_000).is_err());
        assert!(consumer.try_save_cl_snapshot(&pool).is_err());
    }

    #[test]
    fn test_save_snapshot_fails_when_uninitialized() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let pool = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);

        assert!(consumer.try_save_snapshot(&pool).is_err());
    }

    #[test]
    fn test_initialize_is_idempotent_guard() {
        let env = Env::default();
        env.mock_all_auths();

        let keeper = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);

        consumer.initialize(&keeper);
        assert_eq!(consumer.get_keeper(), keeper);
        assert_eq!(
            consumer.try_initialize(&Address::generate(&env)),
            Err(Ok(TwapError::AlreadyInitialized))
        );
    }

    #[test]
    fn test_delete_snapshot_emits_event() {
        let env = Env::default();
        env.mock_all_auths();
        let keeper = Address::generate(&env);
        let pool = Address::generate(&env);
        let ledger_ts = 100u64;
        env.ledger().set_timestamp(ledger_ts);
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);

        // Manually write snapshot to storage
        let snapshot = PriceSnapshot {
            cum_a: 100,
            cum_b: 100,
            pool_ts: ledger_ts,
        };
        env.as_contract(&consumer_addr, || {
            let key = DataKey::Snapshot(pool.clone(), ledger_ts);
            env.storage().persistent().set(&key, &snapshot);
            let mut ts_vec = Vec::new(&env);
            ts_vec.push_back(ledger_ts);
            env.storage()
                .persistent()
                .set(&DataKey::SnapshotTimestamps(pool.clone()), &ts_vec);
        });

        consumer.delete_snapshot(&pool, &ledger_ts);

        let events = env.events().all();
        let event = events.last().unwrap();
        let (contract_id, topics, data) = event;

        assert_eq!(contract_id, consumer_addr);
        let mut expected_topics: Vec<soroban_sdk::Val> = Vec::new(&env);
        expected_topics.push_back(symbol_short!("snap_del").into_val(&env));
        expected_topics.push_back(pool.clone().into_val(&env));
        assert_eq!(topics, expected_topics);
        // Issue #920: the payload is version-stamped as `(EVENT_SCHEMA_VERSION, T)`.
        let (version, data_ts): (u32, u64) = data.into_val(&env);
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data_ts, ledger_ts);
    }

    #[test]
    fn test_save_cl_snapshot_registers_pool_in_tracked_pools() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let cl_addr = env.register_contract(None, MockClPool);

        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);

        assert_eq!(consumer.get_tracked_pools().len(), 0);

        consumer.save_cl_snapshot(&cl_addr);

        let tracked = consumer.get_tracked_pools();
        assert_eq!(tracked.len(), 1);
        assert!(tracked.contains(&cl_addr));

        env.ledger().set_timestamp(10_060);
        consumer.save_cl_snapshot(&cl_addr);
        assert_eq!(consumer.get_tracked_pools().len(), 1);
    }

    // ── Bounty #690: Retention Policy & Bounded Pruning Tests ───────────────

    #[test]
    fn test_retention_policy_sensible_default() {
        let env = Env::default();
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);

        let default_policy = consumer.get_retention_policy();
        assert_eq!(
            default_policy.max_age_seconds,
            TwapConsumer::DEFAULT_MAX_AGE_SECONDS
        );
        assert_eq!(default_policy.max_snapshots_per_pool, 0);
    }

    #[test]
    fn test_set_retention_policy_valid_and_get() {
        let env = Env::default();
        env.mock_all_auths();
        let keeper = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);

        let policy = RetentionPolicy {
            max_age_seconds: 100_000,
            max_snapshots_per_pool: 50,
        };
        consumer.set_retention_policy(&keeper, &policy);
        assert_eq!(consumer.get_retention_policy(), policy);

        // 0 max_age_seconds disables age pruning and is valid
        let disabled_age_policy = RetentionPolicy {
            max_age_seconds: 0,
            max_snapshots_per_pool: 100,
        };
        consumer.set_retention_policy(&keeper, &disabled_age_policy);
        assert_eq!(consumer.get_retention_policy(), disabled_age_policy);
    }

    #[test]
    fn test_set_retention_policy_rejects_shorter_than_longest_window() {
        let env = Env::default();
        env.mock_all_auths();
        let keeper = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);

        let invalid_policy = RetentionPolicy {
            max_age_seconds: TwapConsumer::LONGEST_TWAP_WINDOW - 1,
            max_snapshots_per_pool: 100,
        };
        let res = consumer.try_set_retention_policy(&keeper, &invalid_policy);
        assert_eq!(res, Err(Ok(TwapError::InvalidRetentionPolicy)));
    }

    #[test]
    fn test_set_retention_policy_requires_auth() {
        let env = Env::default();
        env.mock_all_auths();
        let keeper = Address::generate(&env);
        let imposter = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);

        let policy = RetentionPolicy {
            max_age_seconds: 100_000,
            max_snapshots_per_pool: 50,
        };
        let res = consumer.try_set_retention_policy(&imposter, &policy);
        assert_eq!(res, Err(Ok(TwapError::Unauthorized)));
    }

    #[test]
    fn test_get_snapshot_count_and_list_timestamps() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let (pool, consumer) =
            setup_pool_and_consumer(&env, &admin, 2_000_000_i128, 2_000_000_i128);

        assert_eq!(consumer.get_snapshot_count(&pool), 1);

        env.ledger().set_timestamp(10_060);
        consumer.save_snapshot(&pool);
        env.ledger().set_timestamp(10_120);
        consumer.save_snapshot(&pool);

        assert_eq!(consumer.get_snapshot_count(&pool), 3);

        let full_list = consumer.list_snapshot_timestamps(&pool, &0u32, &10u32);
        assert_eq!(full_list.len(), 3);
        assert_eq!(full_list.get(0).unwrap(), 0);
        assert_eq!(full_list.get(1).unwrap(), 10_060);
        assert_eq!(full_list.get(2).unwrap(), 10_120);

        // Paginated queries
        let page1 = consumer.list_snapshot_timestamps(&pool, &0u32, &2u32);
        assert_eq!(page1.len(), 2);
        assert_eq!(page1.get(0).unwrap(), 0);
        assert_eq!(page1.get(1).unwrap(), 10_060);

        let page2 = consumer.list_snapshot_timestamps(&pool, &2u32, &2u32);
        assert_eq!(page2.len(), 1);
        assert_eq!(page2.get(0).unwrap(), 10_120);

        let out_of_bounds = consumer.list_snapshot_timestamps(&pool, &10u32, &5u32);
        assert_eq!(out_of_bounds.len(), 0);
    }

    #[test]
    fn test_get_snapshots_range() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let (pool, consumer) =
            setup_pool_and_consumer(&env, &admin, 2_000_000_i128, 2_000_000_i128);

        env.ledger().set_timestamp(10_000);
        consumer.save_snapshot(&pool);
        env.ledger().set_timestamp(10_060);
        consumer.save_snapshot(&pool);
        env.ledger().set_timestamp(10_120);
        consumer.save_snapshot(&pool);

        let in_range = consumer.get_snapshots(&pool, &10_000, &10_060, &10);
        assert_eq!(in_range.len(), 2);
        assert_eq!(in_range.get(0).unwrap().0, 10_000);
        assert_eq!(in_range.get(1).unwrap().0, 10_060);

        let limited = consumer.get_snapshots(&pool, &10_000, &10_120, &1);
        assert_eq!(limited.len(), 1);
        assert_eq!(limited.get(0).unwrap().0, 10_000);
    }

    #[test]
    fn test_prune_snapshots_boundary_exact() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let pool = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);

        let policy = RetentionPolicy {
            max_age_seconds: 100_000,
            max_snapshots_per_pool: 0,
        };
        consumer.set_retention_policy(&admin, &policy);

        // Helper to manually insert snapshot
        let insert_snapshot = |ts: u64| {
            env.as_contract(&consumer_addr, || {
                let snap = PriceSnapshot {
                    cum_a: 1000,
                    cum_b: 1000,
                    pool_ts: ts,
                };
                env.storage()
                    .persistent()
                    .set(&DataKey::Snapshot(pool.clone(), ts), &snap);
                let mut ts_vec: Vec<u64> = env
                    .storage()
                    .persistent()
                    .get(&DataKey::SnapshotTimestamps(pool.clone()))
                    .unwrap_or_else(|| Vec::new(&env));
                ts_vec.push_back(ts);
                env.storage()
                    .persistent()
                    .set(&DataKey::SnapshotTimestamps(pool.clone()), &ts_vec);
            });
        };

        // Snapshots at timestamps: 10_000, 10_001
        insert_snapshot(10_000);
        insert_snapshot(10_001);

        // At current_ts = 110_000:
        // ts 10_000: age is 110_000 - 10_000 = 100_000 >= max_age_seconds (ELIGIBLE)
        // ts 10_001: age is 110_000 - 10_001 = 99_999 < max_age_seconds (INSIDE RETENTION WINDOW, NOT ELIGIBLE)
        env.ledger().set_timestamp(110_000);

        let removed = consumer.prune_snapshots(&pool, &10);
        assert_eq!(
            removed, 1,
            "Exactly 1 snapshot on the boundary should be pruned"
        );

        let remaining = consumer.list_snapshot_timestamps(&pool, &0, &10);
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining.get(0).unwrap(),
            10_001,
            "Snapshot inside retention window must remain"
        );
    }

    #[test]
    fn test_prune_snapshots_bounded_count() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let pool = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);

        let policy = RetentionPolicy {
            max_age_seconds: 100_000,
            max_snapshots_per_pool: 0,
        };
        consumer.set_retention_policy(&admin, &policy);

        env.as_contract(&consumer_addr, || {
            let mut ts_vec = Vec::new(&env);
            for ts in 1..=10u64 {
                let snap = PriceSnapshot {
                    cum_a: 1000,
                    cum_b: 1000,
                    pool_ts: ts,
                };
                env.storage()
                    .persistent()
                    .set(&DataKey::Snapshot(pool.clone(), ts), &snap);
                ts_vec.push_back(ts);
            }
            env.storage()
                .persistent()
                .set(&DataKey::SnapshotTimestamps(pool.clone()), &ts_vec);
        });

        // Advance time so all 10 are age-eligible
        env.ledger().set_timestamp(200_000);

        // prune_snapshots with max_to_remove = 5
        let removed = consumer.prune_snapshots(&pool, &5);
        assert_eq!(removed, 5);
        assert_eq!(consumer.get_snapshot_count(&pool), 5);

        let remaining = consumer.list_snapshot_timestamps(&pool, &0, &10);
        assert_eq!(remaining.get(0).unwrap(), 6);
        assert_eq!(remaining.get(4).unwrap(), 10);
    }

    #[test]
    fn test_prune_snapshots_by_max_snapshots_per_pool() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let pool = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);

        // Count-based policy: keep at most 3 snapshots, age-based disabled (0)
        let policy = RetentionPolicy {
            max_age_seconds: 0,
            max_snapshots_per_pool: 3,
        };
        consumer.set_retention_policy(&admin, &policy);

        env.as_contract(&consumer_addr, || {
            let mut ts_vec = Vec::new(&env);
            for ts in 1..=6u64 {
                let snap = PriceSnapshot {
                    cum_a: 1000,
                    cum_b: 1000,
                    pool_ts: ts,
                };
                env.storage()
                    .persistent()
                    .set(&DataKey::Snapshot(pool.clone(), ts), &snap);
                ts_vec.push_back(ts);
            }
            env.storage()
                .persistent()
                .set(&DataKey::SnapshotTimestamps(pool.clone()), &ts_vec);
        });

        // 6 exist, max is 3 -> 3 eligible
        let removed = consumer.prune_snapshots(&pool, &10);
        assert_eq!(removed, 3);
        assert_eq!(consumer.get_snapshot_count(&pool), 3);

        let remaining = consumer.list_snapshot_timestamps(&pool, &0, &10);
        assert_eq!(remaining.get(0).unwrap(), 4);
        assert_eq!(remaining.get(1).unwrap(), 5);
        assert_eq!(remaining.get(2).unwrap(), 6);
    }

    #[test]
    fn test_prune_all_across_tracked_pools_with_fault_isolation() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let pool1 = Address::generate(&env);
        let pool2 = Address::generate(&env);
        let pool_empty = Address::generate(&env);

        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);

        let policy = RetentionPolicy {
            max_age_seconds: 100_000,
            max_snapshots_per_pool: 0,
        };
        consumer.set_retention_policy(&admin, &policy);

        env.as_contract(&consumer_addr, || {
            let mut tracked = Vec::new(&env);
            tracked.push_back(pool1.clone());
            tracked.push_back(pool_empty.clone());
            tracked.push_back(pool2.clone());
            env.storage()
                .persistent()
                .set(&DataKey::TrackedPoolsPersistent, &tracked);

            let mut ts1: Vec<u64> = Vec::new(&env);
            ts1.push_back(100);
            ts1.push_back(200);
            env.storage()
                .persistent()
                .set(&DataKey::SnapshotTimestamps(pool1.clone()), &ts1);

            let mut ts2: Vec<u64> = Vec::new(&env);
            ts2.push_back(100);
            ts2.push_back(200);
            ts2.push_back(300);
            env.storage()
                .persistent()
                .set(&DataKey::SnapshotTimestamps(pool2.clone()), &ts2);
        });

        env.ledger().set_timestamp(200_000);

        let total_removed = consumer.prune_all(&2);
        // pool1: 2 removed, pool_empty: 0 removed (fault isolated), pool2: 2 removed -> total 4
        assert_eq!(total_removed, 4);
        assert_eq!(consumer.get_snapshot_count(&pool1), 0);
        assert_eq!(consumer.get_snapshot_count(&pool_empty), 0);
        assert_eq!(consumer.get_snapshot_count(&pool2), 1);
    }

    #[test]
    fn test_amortized_pruning_in_save_snapshot() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let (pool, consumer) =
            setup_pool_and_consumer(&env, &admin, 2_000_000_i128, 2_000_000_i128);

        // Retention policy: cap 2 snapshots per pool
        let policy = RetentionPolicy {
            max_age_seconds: 0,
            max_snapshots_per_pool: 2,
        };
        consumer.set_retention_policy(&admin, &policy);

        env.ledger().set_timestamp(10_060);
        consumer.save_snapshot(&pool);
        assert_eq!(consumer.get_snapshot_count(&pool), 2);

        // Saving a 3rd snapshot triggers amortised pruning (up to 2), keeping count at max 2
        env.ledger().set_timestamp(10_120);
        consumer.save_snapshot(&pool);
        assert_eq!(consumer.get_snapshot_count(&pool), 2);

        let timestamps = consumer.list_snapshot_timestamps(&pool, &0, &10);
        assert_eq!(timestamps.len(), 2);
        assert_eq!(timestamps.get(0).unwrap(), 10_060);
        assert_eq!(timestamps.get(1).unwrap(), 10_120);
    }

    #[test]
    fn test_delete_snapshot_nonexistent_returns_error_and_emits_no_event() {
        let env = Env::default();
        env.mock_all_auths();
        let keeper = Address::generate(&env);
        let pool = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);

        let initial_events = env.events().all().len();

        let res = consumer.try_delete_snapshot(&pool, &999_999);
        assert_eq!(res, Err(Ok(TwapError::NoSnapshotFound)));

        // Verify no event was emitted for missing snapshot
        let final_events = env.events().all().len();
        assert_eq!(initial_events, final_events);
    }

    #[test]
    fn test_index_consistency_interleaved_writes_and_prunes() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let (pool, consumer) =
            setup_pool_and_consumer(&env, &admin, 2_000_000_i128, 2_000_000_i128);

        let policy = RetentionPolicy {
            max_age_seconds: 100_000,
            max_snapshots_per_pool: 0,
        };
        consumer.set_retention_policy(&admin, &policy);

        env.ledger().set_timestamp(10_000);
        consumer.save_snapshot(&pool);
        env.ledger().set_timestamp(20_000);
        consumer.save_snapshot(&pool);

        // Prune older than 100_000 at ts=115_000
        env.ledger().set_timestamp(115_000);
        let removed = consumer.prune_snapshots(&pool, &10);
        // Snapshots: 0, 10_000 are pruned. 20_000 remains (age 95_000).
        assert_eq!(removed, 2);

        // Interleave new write, just short of the 20_000 snapshot's age
        // crossing the retention boundary so opportunistic amortized
        // pruning inside save_snapshot does not also remove it here.
        env.ledger().set_timestamp(119_999);
        consumer.save_snapshot(&pool);

        let timestamps = consumer.list_snapshot_timestamps(&pool, &0, &10);
        assert_eq!(timestamps.len(), 2);
        assert_eq!(timestamps.get(0).unwrap(), 20_000);
        assert_eq!(timestamps.get(1).unwrap(), 119_999);
        assert!(
            timestamps.get(0).unwrap() < timestamps.get(1).unwrap(),
            "Index must remain sorted"
        );
    }

    #[test]
    fn test_twap_reads_succeed_after_pruning() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let (pool, consumer) =
            setup_pool_and_consumer(&env, &admin, 2_000_000_i128, 2_000_000_i128);
        let amm = AmmPoolClient::new(&env, &pool);

        // Save a snapshot in the recent window at ts=110_000, before tightening
        // the retention policy, so opportunistic amortized pruning inside this
        // call (still under the generous default policy) doesn't already
        // remove the ts=0 snapshot ahead of the explicit prune below.
        //
        // A tiny swap checkpoints the AMM's TWAP accumulator at this
        // timestamp — without any reserve-mutating call, the pool's
        // cumulative-price timestamp stays frozen at ts=0 and the later TWAP
        // read below would see zero elapsed time.
        env.ledger().set_timestamp(110_000);
        let info = amm.get_info();
        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &info.token_a).mint(&trader, &1_000_i128);
        amm.swap(&trader, &info.token_a, &1_000_i128, &0_i128, &110_000_u64);
        consumer.save_snapshot(&pool);

        let policy = RetentionPolicy {
            max_age_seconds: 100_000,
            max_snapshots_per_pool: 0,
        };
        consumer.set_retention_policy(&admin, &policy);

        // Advance to 110_060 and prune old snapshot at ts=0
        env.ledger().set_timestamp(110_060);
        let removed = consumer.prune_snapshots(&pool, &10);
        assert_eq!(removed, 1);

        // Another tiny checkpointing swap so the AMM's live cumulative-price
        // timestamp advances past the surviving ts=110_000 snapshot;
        // otherwise the read below would see zero elapsed time between the
        // floor snapshot and "now".
        StellarAssetClient::new(&env, &info.token_a).mint(&trader, &1_000_i128);
        amm.swap(&trader, &info.token_a, &1_000_i128, &0_i128, &110_060_u64);

        // TWAP read over recent window (60s) still succeeds against the
        // surviving ts=110_000 snapshot; the tiny swaps barely move the
        // price off its initial 1:1 ratio.
        let twap = consumer.get_twap_price(&pool, &60_u64);
        assert!(
            (999_000..=1_001_000).contains(&twap),
            "twap {} out of expected range",
            twap
        );

        let (twap_a, twap_b) = consumer.get_twap_both(&pool, &60_u64);
        assert!(
            (999_000..=1_001_000).contains(&twap_a),
            "twap_a {} out of expected range",
            twap_a
        );
        assert!(
            (999_000..=1_001_000).contains(&twap_b),
            "twap_b {} out of expected range",
            twap_b
        );
    }

    #[test]
    fn test_prune_snapshots_emits_pruned_event() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let pool = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);

        let policy = RetentionPolicy {
            max_age_seconds: 100_000,
            max_snapshots_per_pool: 0,
        };
        consumer.set_retention_policy(&admin, &policy);

        env.as_contract(&consumer_addr, || {
            let mut ts_vec = Vec::new(&env);
            for ts in [1000u64, 2000u64, 200_000u64] {
                let snap = PriceSnapshot {
                    cum_a: 1000,
                    cum_b: 1000,
                    pool_ts: ts,
                };
                env.storage()
                    .persistent()
                    .set(&DataKey::Snapshot(pool.clone(), ts), &snap);
                ts_vec.push_back(ts);
            }
            env.storage()
                .persistent()
                .set(&DataKey::SnapshotTimestamps(pool.clone()), &ts_vec);
        });

        env.ledger().set_timestamp(250_000);
        // At 250_000: ts 1000 and 2000 are older than 100_000s; ts 200_000 is 50_000s old (remains)
        let removed = consumer.prune_snapshots(&pool, &10);
        assert_eq!(removed, 2);

        let events = env.events().all();
        let event = events.last().unwrap();
        let (contract_id, topics, data) = event;

        assert_eq!(contract_id, consumer_addr);
        let mut expected_topics: Vec<soroban_sdk::Val> = Vec::new(&env);
        expected_topics.push_back(symbol_short!("pruned").into_val(&env));
        expected_topics.push_back(pool.clone().into_val(&env));
        assert_eq!(topics, expected_topics);

        // Issue #920: the payload is version-stamped as `(EVENT_SCHEMA_VERSION, T)`.
        let (version, (count_val, oldest_ts_val)): (u32, (u32, u64)) = data.into_val(&env);
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(count_val, 2);
        assert_eq!(oldest_ts_val, 200_000);
    }

    // ── Issue #910: instance storage TTL is never extended ──────────────────
    //
    // `twap_consumer` used to extend only its persistent snapshot entries,
    // never the instance entry holding the keeper address and retention
    // policy. A low-traffic oracle consumer that goes unread for long
    // enough would let that instance entry's TTL lapse and get archived,
    // trapping every subsequent call until someone restores it. These tests
    // pin `extend_instance_ttl` in place on the read and write entrypoints.

    fn instance_ttl(env: &Env, consumer: &TwapConsumerClient<'_>) -> u32 {
        env.as_contract(&consumer.address, || env.storage().instance().get_ttl())
    }

    /// Advances the ledger sequence number far enough that the instance
    /// entry's remaining TTL drops below `INSTANCE_TTL_THRESHOLD`, simulating
    /// a long quiet stretch between calls to a sparsely-read oracle consumer.
    fn lower_instance_ttl_below_threshold(env: &Env, consumer: &TwapConsumerClient<'_>) {
        env.ledger().with_mut(|l| {
            l.sequence_number +=
                TwapConsumer::INSTANCE_TTL_BUMP_TO - TwapConsumer::INSTANCE_TTL_THRESHOLD + 1
        });
        let ttl = instance_ttl(env, consumer);
        assert!(
            ttl < TwapConsumer::INSTANCE_TTL_THRESHOLD,
            "test setup should lower instance TTL below the threshold, got {ttl}"
        );
    }

    fn assert_instance_ttl_bumped(env: &Env, consumer: &TwapConsumerClient<'_>) {
        let ttl = instance_ttl(env, consumer);
        assert!(
            ttl >= TwapConsumer::INSTANCE_TTL_BUMP_TO - 1,
            "instance TTL {ttl} should be bumped toward INSTANCE_TTL_BUMP_TO"
        );
    }

    #[test]
    fn test_initialize_extends_instance_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);

        consumer.initialize(&admin);

        assert_instance_ttl_bumped(&env, &consumer);
    }

    /// A read-only entrypoint (`get_keeper`) still restores a lapsed
    /// instance TTL — this is the failure mode the issue calls out: a
    /// read-only path is often the only traffic this contract sees for long
    /// stretches, so it must extend the TTL too, not just the writes.
    #[test]
    fn test_get_keeper_restores_lapsed_instance_ttl_and_still_responds() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);

        lower_instance_ttl_below_threshold(&env, &consumer);

        // The call must still succeed rather than trap on an archived
        // instance entry, and it must restore the TTL for the next caller.
        let keeper = consumer.get_keeper();
        assert_eq!(keeper, admin);
        assert_instance_ttl_bumped(&env, &consumer);
    }

    /// A state-mutating entrypoint (`set_retention_policy`) restores a
    /// lapsed instance TTL as its first statement, before the keeper check
    /// even runs.
    #[test]
    fn test_set_retention_policy_restores_lapsed_instance_ttl_and_still_responds() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwapConsumer);
        let consumer = TwapConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);

        lower_instance_ttl_below_threshold(&env, &consumer);

        // Must still succeed rather than trap on an archived instance entry.
        consumer.set_retention_policy(
            &admin,
            &RetentionPolicy {
                max_age_seconds: TwapConsumer::LONGEST_TWAP_WINDOW,
                max_snapshots_per_pool: 100,
            },
        );

        assert_instance_ttl_bumped(&env, &consumer);
        assert_eq!(consumer.get_retention_policy().max_snapshots_per_pool, 100);
    }
}
