#![no_std]

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, symbol_short, Address,
    Env, Vec,
};

#[contractclient(name = "AmmPoolOracleClient")]
pub trait AmmPoolOracle {
    fn get_price_cumulative(env: Env) -> (i128, i128, u64);
    fn get_info(env: Env) -> AmmPoolInfo;
}

#[contractclient(name = "ClPoolOracleClient")]
pub trait ClPoolOracle {
    fn get_tick_cumulative(env: Env) -> (i64, u64);
    fn observe(env: Env, seconds_ago: u64) -> i64;
}

#[contractclient(name = "FactoryRegistryClient")]
pub trait FactoryRegistry {
    fn get_pool_tokens(env: Env, pool: Address) -> Option<(Address, Address)>;
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
    /// A cross-contract call into a pool's oracle interface failed: the pool
    /// does not implement the function its tracked type implies
    /// (`get_price_cumulative` for `Amm`, `get_tick_cumulative` for `Cl`),
    /// the address is not a contract, or the callee panicked. Reported
    /// instead of trapping so a caller can tell a bad pool from a broken
    /// consumer (#964).
    CrossContractCallFailed = 14,
    SnapshotRateLimited = 15,
    PoolNotRegistered = 16,
}

#[contracttype]
pub enum DataKey {
    Keeper,
    /// Legacy tracked-pool list: a bare `Vec<Address>` with no pool type,
    /// written by contract versions before #964. Read only as a fallback when
    /// `TrackedPools` is absent; every entry is interpreted as `PoolType::Amm`,
    /// which is how `get_twap_all` read them. Removed on the first write of
    /// the typed list.
    TrackedPoolsPersistent,
    /// Every retained snapshot for a pool, in one entry, sorted by ascending
    /// (deduplicated) ledger timestamp. Readers binary-search it for the most
    /// recent snapshot at or before an arbitrary `then_ts` (issue #469).
    ///
    /// Keyed by pool only (issue #985). Snapshots used to live under
    /// `Snapshot(pool, ledger_timestamp)`, but a Soroban transaction's
    /// footprint is fixed at simulation, against an earlier ledger with an
    /// earlier timestamp, so on a real network every save wrote a key outside
    /// its footprint and trapped. Reads had the same flaw: the snapshot they
    /// loaded was chosen by the current timestamp. No storage key in this
    /// contract may depend on the ledger timestamp or sequence;
    /// `scripts/check_storage_keys.sh` enforces that in CI.
    Snapshots(Address),
    RetentionPolicy,
    Factory,
    MinSnapshotInterval,
    /// Typed tracked-pool list (`Vec<TrackedPool>`), replacing the untyped
    /// `TrackedPoolsPersistent` (#964).
    TrackedPools,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AmmPoolInfo {
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

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TwapResult {
    pub twap: i128,
    pub window_used: u64,
    pub snapshot_timestamp: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClTwapResult {
    pub average_tick: i64,
    pub window_used: u64,
    pub snapshot_timestamp: u64,
}

/// Which oracle interface a tracked pool exposes, and therefore which TWAP
/// path reads it. Mirrors `twal_consumer::PoolType`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PoolType {
    /// Constant-product pool: `get_price_cumulative`, read by `get_twap_price`.
    Amm,
    /// Concentrated-liquidity pool: `get_tick_cumulative`, read by `get_cl_twap`.
    Cl,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackedPool {
    pub address: Address,
    pub pool_type: PoolType,
}

/// One pool's result from `get_twap_all`, tagged with the pool's type because
/// the two kinds of pool report different quantities:
///
/// * `Amm`: `twap` is the time-weighted price from `get_twap_price`.
/// * `Cl`: `twap` is the time-weighted mean tick from `get_cl_twap`, widened
///   losslessly from `i64`. Convert it to a price with `1.0001^tick`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TwapEntry {
    pub pool: Address,
    pub pool_type: PoolType,
    pub twap: i128,
}

/// One retained snapshot as stored under [`DataKey::Snapshots`]:
/// `(ledger_ts, cum_a, cum_b, pool_ts)`. A tuple rather than
/// [`PriceSnapshot`] because it encodes at roughly half the size, which is
/// what lets [`TwapConsumer::MAX_SNAPSHOTS_PER_POOL`] snapshots fit in a
/// single ledger entry.
type SnapshotEntry = (u64, i128, i128, u64);

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

    /// Configure the factory used to validate permissionless snapshot calls.
    /// Existing keeper-authorized snapshot entrypoints are unchanged.
    pub fn configure_public_snapshots(
        env: Env,
        factory: Address,
        min_interval_seconds: u64,
    ) -> Result<(), TwapError> {
        Self::extend_instance_ttl(&env);
        Self::require_keeper(&env)?;
        if min_interval_seconds == 0 {
            return Err(TwapError::InvalidRetentionPolicy);
        }
        env.storage().instance().set(&DataKey::Factory, &factory);
        env.storage()
            .instance()
            .set(&DataKey::MinSnapshotInterval, &min_interval_seconds);
        Ok(())
    }

    fn require_registered_pool(env: &Env, pool: &Address) -> Result<(), TwapError> {
        let factory: Address = env
            .storage()
            .instance()
            .get(&DataKey::Factory)
            .ok_or(TwapError::PoolNotRegistered)?;
        if FactoryRegistryClient::new(env, &factory)
            .get_pool_tokens(pool)
            .is_none()
        {
            return Err(TwapError::PoolNotRegistered);
        }
        let min_interval: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MinSnapshotInterval)
            .unwrap_or(300);
        if let Some(last) = Self::load_snapshots(env, pool).last() {
            if env.ledger().timestamp().saturating_sub(last.0) < min_interval {
                return Err(TwapError::SnapshotRateLimited);
            }
        }
        Ok(())
    }

    /// Sets the snapshot retention policy. Caller must be the keeper/admin.
    /// Rejects policies with `0 < max_age_seconds < LONGEST_TWAP_WINDOW` or
    /// `max_snapshots_per_pool > MAX_SNAPSHOTS_PER_POOL`.
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
        if policy.max_snapshots_per_pool > Self::MAX_SNAPSHOTS_PER_POOL {
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

    /// Returns the number of snapshots retained for `pool`.
    pub fn get_snapshot_count(env: Env, pool: Address) -> u32 {
        Self::extend_instance_ttl(&env);
        Self::load_snapshots(&env, &pool).len()
    }

    /// Ledger timestamp of the newest retained snapshot, or zero when empty.
    pub fn get_latest_snapshot_timestamp(env: Env, pool: Address) -> u64 {
        Self::extend_instance_ttl(&env);
        Self::load_snapshots(&env, &pool)
            .last()
            .map(|entry| entry.0)
            .unwrap_or(0)
    }

    /// Returns a paginated slice of snapshot timestamps for `pool`.
    pub fn list_snapshot_timestamps(env: Env, pool: Address, offset: u32, limit: u32) -> Vec<u64> {
        Self::extend_instance_ttl(&env);
        let snapshots = Self::load_snapshots(&env, &pool);
        let len = snapshots.len();
        let mut result = Vec::new(&env);
        if offset >= len || limit == 0 {
            return result;
        }
        let end = offset.saturating_add(limit).min(len);
        for i in offset..end {
            result.push_back(snapshots.get(i).unwrap().0);
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
        let snapshots = Self::load_snapshots(&env, &pool);
        let mut result = Vec::new(&env);
        let max_items = if limit == 0 { u32::MAX } else { limit };
        for entry in snapshots.iter() {
            if result.len() >= max_items {
                break;
            }
            if entry.0 >= from_ts && entry.0 <= to_ts {
                result.push_back((entry.0, Self::to_price_snapshot(&entry)));
            }
        }
        result
    }

    pub fn save_snapshot(env: Env, pool: Address) -> Result<(), TwapError> {
        Self::extend_instance_ttl(&env);
        Self::require_keeper(&env)?;
        let (cum_a, cum_b, pool_ts) = Self::amm_price_cumulative(&env, &pool)?;
        Self::record_snapshot(&env, &pool, PoolType::Amm, cum_a, cum_b, pool_ts);
        Ok(())
    }

    /// Save an AMM snapshot without keeper authorization for a factory pool.
    /// Calls are rate-limited by `configure_public_snapshots`.
    pub fn save_snapshot_public(env: Env, pool: Address) -> Result<(), TwapError> {
        Self::extend_instance_ttl(&env);
        Self::require_registered_pool(&env, &pool)?;
        let (cum_a, cum_b, pool_ts) = Self::amm_price_cumulative(&env, &pool)?;
        Self::record_snapshot(&env, &pool, PoolType::Amm, cum_a, cum_b, pool_ts);
        Ok(())
    }

    /// Deletes a price snapshot from persistent storage.
    /// Returns `TwapError::NoSnapshotFound` and emits no event if the snapshot does not exist.
    pub fn delete_snapshot(env: Env, pool: Address, ledger_ts: u64) -> Result<(), TwapError> {
        Self::extend_instance_ttl(&env);
        Self::require_keeper(&env)?;
        let mut snapshots = Self::load_snapshots(&env, &pool);
        let index = snapshots
            .iter()
            .position(|entry| entry.0 == ledger_ts)
            .ok_or(TwapError::NoSnapshotFound)?;
        snapshots.remove(index as u32);
        Self::store_snapshots(&env, &pool, &snapshots);
        soroban_amm_sdk::emit_versioned_event!(&env, (symbol_short!("snap_del"), pool), ledger_ts);
        Ok(())
    }

    /// Hard ceiling on snapshots retained per pool, whatever the retention
    /// policy says. All of a pool's snapshots share one ledger entry, and
    /// ledger entries are size-limited (64 KiB on public networks); at about
    /// 76 bytes per snapshot this keeps a full entry near 39 KiB. It still
    /// covers [`Self::LONGEST_TWAP_WINDOW`] for a keeper saving as often as
    /// every 3 minutes. Once reached, each save drops the oldest snapshot.
    pub const MAX_SNAPSHOTS_PER_POOL: u32 = 512;

    fn load_snapshots(env: &Env, pool: &Address) -> Vec<SnapshotEntry> {
        env.storage()
            .persistent()
            .get(&DataKey::Snapshots(pool.clone()))
            .unwrap_or_else(|| Vec::new(env))
    }

    fn store_snapshots(env: &Env, pool: &Address, snapshots: &Vec<SnapshotEntry>) {
        let key = DataKey::Snapshots(pool.clone());
        env.storage().persistent().set(&key, snapshots);
        env.storage().persistent().extend_ttl(
            &key,
            Self::SNAPSHOT_TTL_LEDGERS / 2,
            Self::SNAPSHOT_TTL_LEDGERS,
        );
    }

    fn to_price_snapshot(entry: &SnapshotEntry) -> PriceSnapshot {
        PriceSnapshot {
            cum_a: entry.1,
            cum_b: entry.2,
            pool_ts: entry.3,
        }
    }

    /// Shared body of `save_snapshot` / `save_cl_snapshot`: record the
    /// snapshot at the current ledger time, prune, and register the pool.
    ///
    /// Every key touched here is fixed given `pool`, so the footprint a
    /// client simulates is the footprint the transaction applies with, even
    /// though the ledger timestamp moves on in between (issue #985).
    fn record_snapshot(
        env: &Env,
        pool: &Address,
        pool_type: PoolType,
        cum_a: i128,
        cum_b: i128,
        pool_ts: u64,
    ) {
        let ledger_ts = env.ledger().timestamp();
        let entry: SnapshotEntry = (ledger_ts, cum_a, cum_b, pool_ts);
        let mut snapshots = Self::load_snapshots(env, pool);
        // Ledger timestamps never decrease, so appending keeps the list
        // sorted. A second save within the same ledger replaces the first.
        match snapshots.last() {
            Some(last) if last.0 == ledger_ts => snapshots.set(snapshots.len() - 1, entry),
            _ => snapshots.push_back(entry),
        }
        // Opportunistic bounded amortised pruning. Each save adds at most one
        // snapshot and this removes up to two, so the count cap always holds.
        Self::prune_list(env, pool, &mut snapshots, Self::AMORTIZED_PRUNE_LIMIT);
        Self::store_snapshots(env, pool, &snapshots);

        Self::register_tracked_pool(env, pool, pool_type);
    }

    /// Reads the tracked-pool set, falling back to the legacy untyped list.
    ///
    /// Migration (#964): contract versions before the typed list stored a
    /// bare `Vec<Address>` under `TrackedPoolsPersistent`. When `TrackedPools`
    /// has not been written yet, that list is read with every entry typed as
    /// `PoolType::Amm`, which is exactly how the old `get_twap_all` read it. A
    /// legacy entry that is really a CL pool is re-typed the next time the
    /// keeper calls `save_cl_snapshot` for it (see `register_tracked_pool`);
    /// until then `get_twap_all` reports it as `CrossContractCallFailed`
    /// instead of trapping.
    fn load_tracked(env: &Env) -> Vec<TrackedPool> {
        if let Some(tracked) = env.storage().persistent().get(&DataKey::TrackedPools) {
            return tracked;
        }
        let legacy: Vec<Address> = env
            .storage()
            .persistent()
            .get(&DataKey::TrackedPoolsPersistent)
            .unwrap_or_else(|| Vec::new(env));
        let mut tracked = Vec::new(env);
        for address in legacy.iter() {
            tracked.push_back(TrackedPool {
                address,
                pool_type: PoolType::Amm,
            });
        }
        tracked
    }

    /// Writes the typed tracked-pool set and drops the legacy untyped list,
    /// completing the migration described on `load_tracked`.
    fn store_tracked(env: &Env, tracked: &Vec<TrackedPool>) {
        let storage = env.storage().persistent();
        storage.set(&DataKey::TrackedPools, tracked);
        storage.extend_ttl(
            &DataKey::TrackedPools,
            Self::SNAPSHOT_TTL_LEDGERS / 2,
            Self::SNAPSHOT_TTL_LEDGERS,
        );
        if storage.has(&DataKey::TrackedPoolsPersistent) {
            storage.remove(&DataKey::TrackedPoolsPersistent);
        }
    }

    /// Adds `pool` to the tracked set as `pool_type`, or updates its type if
    /// it is already tracked as the other one. A pool's type is decided by the
    /// snapshot call that succeeded for it, and each call only succeeds
    /// against the matching oracle interface, so re-typing can only correct a
    /// migrated legacy entry, never corrupt a good one.
    fn register_tracked_pool(env: &Env, pool: &Address, pool_type: PoolType) {
        let mut tracked = Self::load_tracked(env);
        for i in 0..tracked.len() {
            let mut entry = tracked.get(i).unwrap();
            if entry.address == *pool {
                if entry.pool_type == pool_type {
                    return;
                }
                entry.pool_type = pool_type;
                tracked.set(i, entry);
                Self::store_tracked(env, &tracked);
                return;
            }
        }
        tracked.push_back(TrackedPool {
            address: pool.clone(),
            pool_type,
        });
        Self::store_tracked(env, &tracked);
    }

    /// `get_price_cumulative` on an AMM pool, with any failure of the call
    /// itself reported as `CrossContractCallFailed` rather than a host trap.
    fn amm_price_cumulative(env: &Env, pool: &Address) -> Result<(i128, i128, u64), TwapError> {
        match AmmPoolOracleClient::new(env, pool).try_get_price_cumulative() {
            Ok(Ok(v)) => Ok(v),
            _ => Err(TwapError::CrossContractCallFailed),
        }
    }

    /// `get_tick_cumulative` on a CL pool, with any failure of the call itself
    /// reported as `CrossContractCallFailed` rather than a host trap.
    fn cl_tick_cumulative(env: &Env, pool: &Address) -> Result<(i64, u64), TwapError> {
        match ClPoolOracleClient::new(env, pool).try_get_tick_cumulative() {
            Ok(Ok(v)) => Ok(v),
            _ => Err(TwapError::CrossContractCallFailed),
        }
    }

    /// Removes up to `max_to_remove` snapshots from `snapshots` that the
    /// retention policy (or [`Self::MAX_SNAPSHOTS_PER_POOL`]) makes eligible,
    /// oldest first, emitting a `pruned` event if any were removed. Storing
    /// the list is left to the caller.
    fn prune_list(
        env: &Env,
        pool: &Address,
        snapshots: &mut Vec<SnapshotEntry>,
        max_to_remove: u32,
    ) -> u32 {
        let total_count = snapshots.len();
        if max_to_remove == 0 || total_count == 0 {
            return 0;
        }
        let policy = Self::get_retention_policy(env.clone());
        let max_count = if policy.max_snapshots_per_pool > 0 {
            policy
                .max_snapshots_per_pool
                .min(Self::MAX_SNAPSHOTS_PER_POOL)
        } else {
            Self::MAX_SNAPSHOTS_PER_POOL
        };
        let current_ts = env.ledger().timestamp();

        let mut remove_count = 0u32;
        let mut remaining = Vec::new(env);
        for entry in snapshots.iter() {
            let ts = entry.0;
            let remaining_count = total_count - remove_count;

            let age_eligible = policy.max_age_seconds > 0
                && current_ts >= ts.saturating_add(policy.max_age_seconds);
            let count_eligible = remaining_count > max_count;

            if (age_eligible || count_eligible) && remove_count < max_to_remove {
                remove_count += 1;
            } else {
                remaining.push_back(entry);
            }
        }

        if remove_count > 0 {
            *snapshots = remaining;
            let oldest_remaining_ts = snapshots.first().map(|entry| entry.0).unwrap_or(0);
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
        let mut snapshots = Self::load_snapshots(&env, &pool);
        let removed = Self::prune_list(&env, &pool, &mut snapshots, max_to_remove);
        if removed > 0 {
            Self::store_snapshots(&env, &pool, &snapshots);
        }
        removed
    }

    /// Permissionless sweep across all tracked pools, removing up to `max_to_remove_per_pool`
    /// eligible snapshots per pool. Fault-isolated so one pool cannot abort the sweep.
    pub fn prune_all(env: Env, max_to_remove_per_pool: u32) -> u32 {
        Self::extend_instance_ttl(&env);
        let tracked: Vec<Address> = Self::get_tracked_pools(env.clone());
        let mut total_removed = 0u32;
        for i in 0..tracked.len() {
            let pool = tracked.get(i).unwrap();
            let removed = Self::prune_snapshots(env.clone(), pool, max_to_remove_per_pool);
            total_removed = total_removed.saturating_add(removed);
        }
        total_removed
    }

    /// Binary-search the pool's snapshots for the most recent one at or
    /// before `then_ts` (the "floor"). Returns `None` if no snapshot that old
    /// exists.
    fn floor_snapshot_entry(env: &Env, pool: &Address, then_ts: u64) -> Option<SnapshotEntry> {
        let snapshots = Self::load_snapshots(env, pool);
        let mut lo: u32 = 0;
        let mut hi: u32 = snapshots.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if snapshots.get(mid).unwrap().0 <= then_ts {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            None
        } else {
            snapshots.get(lo - 1)
        }
    }

    fn extrapolated_amm_cumulative(
        env: &Env,
        pool: &Address,
    ) -> Result<(i128, i128, u64), TwapError> {
        let (mut cum_a, mut cum_b, pool_ts) = Self::amm_price_cumulative(env, pool)?;
        let now = env.ledger().timestamp();
        if now <= pool_ts {
            return Ok((cum_a, cum_b, now));
        }
        let info = AmmPoolOracleClient::new(env, pool).get_info();
        if info.reserve_a <= 0 || info.reserve_b <= 0 {
            return Err(TwapError::InvalidSpotPrice);
        }
        let elapsed = i128::from(now - pool_ts);
        let price_a = info
            .reserve_b
            .checked_mul(Self::PRICE_SCALE)
            .ok_or(TwapError::InvalidSpotPrice)?
            / info.reserve_a;
        let price_b = info
            .reserve_a
            .checked_mul(Self::PRICE_SCALE)
            .ok_or(TwapError::InvalidSpotPrice)?
            / info.reserve_b;
        cum_a = cum_a
            .checked_add(
                price_a
                    .checked_mul(elapsed)
                    .ok_or(TwapError::InvalidSpotPrice)?,
            )
            .ok_or(TwapError::InvalidSpotPrice)?;
        cum_b = cum_b
            .checked_add(
                price_b
                    .checked_mul(elapsed)
                    .ok_or(TwapError::InvalidSpotPrice)?,
            )
            .ok_or(TwapError::InvalidSpotPrice)?;
        Ok((cum_a, cum_b, now))
    }

    pub fn get_twap_price_detailed(
        env: Env,
        pool: Address,
        window_seconds: u64,
    ) -> Result<TwapResult, TwapError> {
        Self::extend_instance_ttl(&env);
        if window_seconds == 0 {
            return Err(TwapError::ZeroWindow);
        }
        let ledger_ts_now = env.ledger().timestamp();
        if ledger_ts_now < window_seconds {
            return Err(TwapError::InsufficientHistory);
        }
        // Read the pool before looking at history, so a pool that does not
        // implement the AMM oracle interface is reported as such (#964) rather
        // than as having too little history.
        let (cum_a_now, _, _) = Self::extrapolated_amm_cumulative(&env, &pool)?;
        let snapshot = Self::floor_snapshot_entry(&env, &pool, ledger_ts_now - window_seconds)
            .ok_or(TwapError::InsufficientHistory)?;
        let elapsed = ledger_ts_now.saturating_sub(snapshot.0);
        if elapsed == 0 {
            return Err(TwapError::ElapsedZero);
        }
        let delta = (cum_a_now as u128).wrapping_sub(snapshot.1 as u128) as i128;
        Ok(TwapResult {
            twap: delta / i128::from(elapsed),
            window_used: elapsed,
            snapshot_timestamp: snapshot.0,
        })
    }

    pub fn get_twap_price(env: Env, pool: Address, window_seconds: u64) -> Result<i128, TwapError> {
        Ok(Self::get_twap_price_detailed(env, pool, window_seconds)?.twap)
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
        let (cum_a_now, cum_b_now, _) = Self::extrapolated_amm_cumulative(&env, &pool)?;
        let ledger_ts_now = env.ledger().timestamp();
        if ledger_ts_now < window_seconds {
            return Err(TwapError::InsufficientHistory);
        }
        let then_ts = ledger_ts_now - window_seconds;
        let snapshot = Self::floor_snapshot_entry(&env, &pool, then_ts)
            .ok_or(TwapError::InsufficientHistory)?;

        let delta_a = (cum_a_now as u128).wrapping_sub(snapshot.1 as u128) as i128;
        let delta_b = (cum_b_now as u128).wrapping_sub(snapshot.2 as u128) as i128;
        let elapsed = i128::from(ledger_ts_now - snapshot.0);
        if elapsed <= 0 {
            return Err(TwapError::ElapsedZero);
        }
        Ok((delta_a / elapsed, delta_b / elapsed))
    }

    /// Addresses of every tracked pool, of either type. See
    /// `get_tracked_pools_typed` for each pool's type.
    pub fn get_tracked_pools(env: Env) -> Vec<Address> {
        Self::extend_instance_ttl(&env);
        let tracked = Self::load_tracked(&env);
        let mut out = Vec::new(&env);
        for entry in tracked.iter() {
            out.push_back(entry.address);
        }
        out
    }

    /// The tracked-pool set with each pool's type.
    pub fn get_tracked_pools_typed(env: Env) -> Vec<TrackedPool> {
        Self::extend_instance_ttl(&env);
        Self::load_tracked(&env)
    }

    /// TWAP for every tracked pool, dispatched by pool type: `get_twap_price`
    /// for `Amm` pools and `get_cl_twap` for `Cl` pools. Each entry carries
    /// its pool type because the two report different quantities (see
    /// `TwapEntry`).
    ///
    /// Returns the first error encountered, like `twal_consumer::get_twal_all`.
    /// A pool that does not implement the interface its type implies yields
    /// `CrossContractCallFailed` rather than a host trap (#964).
    pub fn get_twap_all(env: Env, window_seconds: u64) -> Result<Vec<TwapEntry>, TwapError> {
        Self::extend_instance_ttl(&env);
        let tracked = Self::load_tracked(&env);
        let mut results = Vec::new(&env);
        for entry in tracked.iter() {
            let twap = match entry.pool_type {
                PoolType::Amm => {
                    Self::get_twap_price(env.clone(), entry.address.clone(), window_seconds)?
                }
                PoolType::Cl => i128::from(Self::get_cl_twap(
                    env.clone(),
                    entry.address.clone(),
                    window_seconds,
                )?),
            };
            results.push_back(TwapEntry {
                pool: entry.address,
                pool_type: entry.pool_type,
                twap,
            });
        }
        Ok(results)
    }

    pub fn get_cl_twap(env: Env, pool: Address, window_seconds: u64) -> Result<i64, TwapError> {
        Ok(Self::get_cl_twap_detailed(env, pool, window_seconds)?.average_tick)
    }

    pub fn get_cl_twap_detailed(
        env: Env,
        pool: Address,
        window_seconds: u64,
    ) -> Result<ClTwapResult, TwapError> {
        Self::extend_instance_ttl(&env);
        if window_seconds == 0 {
            return Err(TwapError::ZeroWindow);
        }
        // `observe(0)` is the cumulative extrapolated to the current ledger
        // time, which is what `elapsed` below is measured against. A pool that
        // does not implement it is a typed error, not a trap (#964).
        let cum_now = match ClPoolOracleClient::new(&env, &pool).try_observe(&0) {
            Ok(Ok(v)) => v,
            _ => return Err(TwapError::CrossContractCallFailed),
        };
        let ledger_ts_now = env.ledger().timestamp();
        if ledger_ts_now < window_seconds {
            return Err(TwapError::InsufficientHistory);
        }
        let then_ts = ledger_ts_now - window_seconds;
        let snapshot = Self::floor_snapshot_entry(&env, &pool, then_ts)
            .ok_or(TwapError::InsufficientHistory)?;

        let cum_then = snapshot.1 as i64;
        let elapsed = ledger_ts_now.saturating_sub(snapshot.0);
        if elapsed == 0 {
            return Err(TwapError::ElapsedZero);
        }
        Ok(ClTwapResult {
            average_tick: (cum_now - cum_then) / elapsed as i64,
            window_used: elapsed,
            snapshot_timestamp: snapshot.0,
        })
    }

    pub fn save_cl_snapshot(env: Env, pool: Address) -> Result<(), TwapError> {
        Self::extend_instance_ttl(&env);
        Self::require_keeper(&env)?;
        let (tick_cum, pool_ts) = Self::cl_tick_cumulative(&env, &pool)?;
        Self::record_snapshot(&env, &pool, PoolType::Cl, tick_cum as i128, 0, pool_ts);
        Ok(())
    }

    /// Save a CL snapshot without keeper authorization for a factory pool.
    pub fn save_cl_snapshot_public(env: Env, pool: Address) -> Result<(), TwapError> {
        Self::extend_instance_ttl(&env);
        Self::require_registered_pool(&env, &pool)?;
        let (tick_cum, pool_ts) = Self::cl_tick_cumulative(&env, &pool)?;
        Self::record_snapshot(&env, &pool, PoolType::Cl, tick_cum as i128, 0, pool_ts);
        Ok(())
    }
}

/// Minimal mock CL pool used by tests only. Satisfies the `ClPoolOracle`
/// interface (`get_tick_cumulative` and `observe`) without requiring the full
/// CL contract.
/// Reports `(1_000, 10_000)` until a test moves it with `set_tick_cumulative`.
#[cfg(test)]
mod mock_cl_pool {
    use soroban_sdk::{contract, contractimpl, symbol_short, Env};

    #[contract]
    pub struct MockClPool;

    #[contractimpl]
    impl MockClPool {
        pub fn get_tick_cumulative(env: Env) -> (i64, u64) {
            env.storage()
                .instance()
                .get(&symbol_short!("tick_cum"))
                .unwrap_or((1_000_i64, 10_000_u64))
        }

        /// Mirrors the CL pool's `observe(0)`: the current cumulative. Only the
        /// present is supported, which is all the consumer asks for.
        pub fn observe(env: Env, seconds_ago: u64) -> i64 {
            assert_eq!(
                seconds_ago, 0,
                "MockClPool only reports the current cumulative"
            );
            Self::get_tick_cumulative(env).0
        }

        pub fn set_tick_cumulative(env: Env, tick_cum: i64, ts: u64) {
            env.storage()
                .instance()
                .set(&symbol_short!("tick_cum"), &(tick_cum, ts));
        }
    }
}

#[cfg(test)]
use mock_cl_pool::{MockClPool, MockClPoolClient};

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

    /// Writes `(ledger_ts, cum, cum, pool_ts = ledger_ts)` snapshots for each
    /// timestamp straight into the consumer's storage, bypassing
    /// `save_snapshot`, so pruning can be tested against arbitrary history.
    fn seed_snapshots(env: &Env, consumer: &Address, pool: &Address, cum: i128, stamps: &[u64]) {
        env.as_contract(consumer, || {
            let mut snapshots: Vec<SnapshotEntry> = env
                .storage()
                .persistent()
                .get(&DataKey::Snapshots(pool.clone()))
                .unwrap_or_else(|| Vec::new(env));
            for &ts in stamps {
                snapshots.push_back((ts, cum, cum, ts));
            }
            env.storage()
                .persistent()
                .set(&DataKey::Snapshots(pool.clone()), &snapshots);
        });
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

        for entry in all_twaps.iter() {
            assert_eq!(entry.pool_type, PoolType::Amm);
            if entry.pool == amm_addr1 {
                assert_eq!(entry.twap, twap1);
            } else {
                assert_eq!(entry.twap, twap2);
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
        seed_snapshots(&env, &consumer_addr, &pool, 100, &[ledger_ts]);

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
        let insert_snapshot = |ts: u64| seed_snapshots(&env, &consumer_addr, &pool, 1000, &[ts]);

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

        seed_snapshots(
            &env,
            &consumer_addr,
            &pool,
            1000,
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
        );

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

        seed_snapshots(&env, &consumer_addr, &pool, 1000, &[1, 2, 3, 4, 5, 6]);

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
        });
        seed_snapshots(&env, &consumer_addr, &pool1, 1000, &[100, 200]);
        seed_snapshots(&env, &consumer_addr, &pool2, 1000, &[100, 200, 300]);

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

        seed_snapshots(&env, &consumer_addr, &pool, 1000, &[1000, 2000, 200_000]);

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

    /// Every contract storage key in the ledger. A Soroban transaction may only
    /// touch keys its simulated footprint lists, so the keys a save writes
    /// must not change with the ledger clock (issue #985).
    fn contract_data_keys(env: &Env) -> std::collections::BTreeSet<soroban_sdk::xdr::LedgerKey> {
        env.to_snapshot()
            .ledger
            .ledger_entries
            .into_iter()
            .map(|(key, _)| *key)
            // Auth nonces (from `mock_all_auths`) are not contract storage.
            .filter(|key| {
                matches!(key, soroban_sdk::xdr::LedgerKey::ContractData(data)
                    if !matches!(data.key, soroban_sdk::xdr::ScVal::LedgerKeyNonce(_)))
            })
            .collect()
    }

    #[test]
    fn save_snapshot_keys_do_not_depend_on_ledger_time() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);
        let admin = Address::generate(&env);
        // The fixture's first save_snapshot establishes the pool's entry.
        let (pool, consumer) = setup_pool_and_consumer(&env, &admin, 1_000_000, 1_000_000);
        let keys_before = contract_data_keys(&env);

        // Saves at later ledger times — what apply sees after a client
        // simulated against an earlier ledger — must write only keys that
        // already exist, never a new one derived from the timestamp.
        for ts in [10_005, 10_060, 99_999] {
            env.ledger().set_timestamp(ts);
            consumer.save_snapshot(&pool);
            assert_eq!(
                contract_data_keys(&env),
                keys_before,
                "save at ts {ts} wrote a new key"
            );
        }
        assert_eq!(consumer.get_snapshot_count(&pool), 4);
    }

    #[test]
    fn save_cl_snapshot_keys_do_not_depend_on_ledger_time() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(20_000);
        let keeper = Address::generate(&env);
        let pool = env.register_contract(None, MockClPool);
        let consumer = TwapConsumerClient::new(&env, &env.register_contract(None, TwapConsumer));
        consumer.initialize(&keeper);
        consumer.save_cl_snapshot(&pool);
        let keys_before = contract_data_keys(&env);

        for ts in [20_001, 20_500] {
            env.ledger().set_timestamp(ts);
            consumer.save_cl_snapshot(&pool);
            assert_eq!(
                contract_data_keys(&env),
                keys_before,
                "save at ts {ts} wrote a new key"
            );
        }
    }

    #[test]
    fn full_history_is_capped_fits_one_entry_and_stays_within_budget() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        env.ledger().set_timestamp(1_000_000);
        let admin = Address::generate(&env);
        let (pool, consumer) = setup_pool_and_consumer(&env, &admin, 1_000_000, 1_000_000);

        let cap = TwapConsumer::MAX_SNAPSHOTS_PER_POOL;
        let saves = cap + 40;
        // One save a minute: well inside the default 7-day age limit, so only
        // the hard count cap prunes.
        for i in 1..saves {
            env.ledger().set_timestamp(1_000_000 + 60 * i as u64);
            consumer.save_snapshot(&pool);
        }
        assert_eq!(consumer.get_snapshot_count(&pool), cap);
        let stamps = consumer.list_snapshot_timestamps(&pool, &0, &1);
        assert_eq!(
            stamps.get(0).unwrap(),
            1_000_000 + 60 * (saves - cap) as u64
        );

        // The whole history is one ledger entry; keep it well under the
        // 64 KiB entry size limit.
        let encoded = env.as_contract(&consumer.address, || {
            let snapshots: Vec<SnapshotEntry> = env
                .storage()
                .persistent()
                .get(&DataKey::Snapshots(pool.clone()))
                .unwrap();
            use soroban_sdk::xdr::ToXdr;
            snapshots.to_xdr(&env).len()
        });
        assert!(
            encoded < 48 * 1024,
            "full history encodes to {encoded} bytes"
        );

        // A save against a full history fits the default (network-sized)
        // resource budget.
        env.budget().reset_default();
        env.ledger().set_timestamp(1_000_000 + 60 * saves as u64);
        consumer.save_snapshot(&pool);
        assert_eq!(consumer.get_snapshot_count(&pool), cap);
    }

    #[test]
    fn retention_count_above_hard_cap_is_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let consumer = TwapConsumerClient::new(&env, &env.register_contract(None, TwapConsumer));
        consumer.initialize(&admin);
        let too_many = RetentionPolicy {
            max_age_seconds: 0,
            max_snapshots_per_pool: TwapConsumer::MAX_SNAPSHOTS_PER_POOL + 1,
        };
        assert_eq!(
            consumer.try_set_retention_policy(&admin, &too_many),
            Err(Ok(TwapError::InvalidRetentionPolicy))
        );
        let at_cap = RetentionPolicy {
            max_age_seconds: 0,
            max_snapshots_per_pool: TwapConsumer::MAX_SNAPSHOTS_PER_POOL,
        };
        consumer.set_retention_policy(&admin, &at_cap);
    }

    #[test]
    fn same_ledger_resave_replaces_and_delete_follows_the_single_entry() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);
        let admin = Address::generate(&env);
        let (pool, consumer) = setup_pool_and_consumer(&env, &admin, 1_000_000, 1_000_000);
        consumer.save_snapshot(&pool);
        assert_eq!(consumer.get_snapshot_count(&pool), 1);

        env.ledger().set_timestamp(10_100);
        consumer.save_snapshot(&pool);
        assert_eq!(consumer.get_snapshot_count(&pool), 2);

        consumer.delete_snapshot(&pool, &10_000);
        let stamps = consumer.list_snapshot_timestamps(&pool, &0, &10);
        assert_eq!(stamps.len(), 1);
        assert_eq!(stamps.get(0).unwrap(), 10_100);
        assert_eq!(
            consumer.try_delete_snapshot(&pool, &10_000),
            Err(Ok(TwapError::NoSnapshotFound))
        );
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

    // ── #964: typed tracked pools ────────────────────────────────────────────

    /// A seeded 1:1 AMM pool plus the pieces needed to move its cumulative.
    struct SeededAmm<'a> {
        address: Address,
        client: AmmPoolClient<'a>,
        token_a: Address,
        sac_a: StellarAssetClient<'a>,
    }

    fn seeded_amm<'a>(env: &'a Env, admin: &Address) -> SeededAmm<'a> {
        let address = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        token::LpTokenClient::new(env, &lp_addr).initialize(
            &address,
            &soroban_sdk::String::from_str(env, "LP"),
            &soroban_sdk::String::from_str(env, "LP"),
            &7u32,
        );
        let (ta, sac_a) = create_sac(env, admin);
        let (tb, sac_b) = create_sac(env, admin);
        let client = AmmPoolClient::new(env, &address);
        client.initialize(
            admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            admin,
            &0_i128,
        );
        let provider = Address::generate(env);
        sac_a.mint(&provider, &2_000_000_i128);
        sac_b.mint(&provider, &2_000_000_i128);
        client.add_liquidity(
            &provider,
            &2_000_000_i128,
            &2_000_000_i128,
            &0_i128,
            &(env.ledger().timestamp() + 10_000),
        );
        SeededAmm {
            address,
            client,
            token_a: ta.address,
            sac_a,
        }
    }

    /// A small swap at the current ledger time, so the pool's price
    /// cumulative advances to it.
    fn poke_amm(env: &Env, amm: &SeededAmm) {
        let whale = Address::generate(env);
        amm.sac_a.mint(&whale, &1_000_i128);
        amm.client.swap(
            &whale,
            &amm.token_a,
            &1_000_i128,
            &0_i128,
            &env.ledger().timestamp(),
        );
    }

    fn new_consumer<'a>(env: &'a Env, admin: &Address) -> TwapConsumerClient<'a> {
        let consumer = TwapConsumerClient::new(env, &env.register_contract(None, TwapConsumer));
        consumer.initialize(admin);
        consumer
    }

    #[test]
    fn test_get_twap_all_returns_mixed_amm_and_cl_entries() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);
        let admin = Address::generate(&env);

        let amm = seeded_amm(&env, &admin);
        let cl_addr = env.register_contract(None, MockClPool);
        let cl = MockClPoolClient::new(&env, &cl_addr);
        let consumer = new_consumer(&env, &admin);

        consumer.save_snapshot(&amm.address);
        consumer.save_cl_snapshot(&cl_addr);

        let typed = consumer.get_tracked_pools_typed();
        assert_eq!(
            typed,
            soroban_sdk::vec![
                &env,
                TrackedPool {
                    address: amm.address.clone(),
                    pool_type: PoolType::Amm,
                },
                TrackedPool {
                    address: cl_addr.clone(),
                    pool_type: PoolType::Cl,
                },
            ]
        );

        // 60 seconds later: the AMM trades at 1:1 and the CL pool has sat at
        // tick -250 the whole time (cumulative 1_000 -> 1_000 - 250 * 60).
        env.ledger().set_timestamp(10_060);
        poke_amm(&env, &amm);
        cl.set_tick_cumulative(&(1_000_i64 - 250 * 60), &10_060_u64);

        let amm_alone = consumer.get_twap_price(&amm.address, &60_u64);
        let cl_alone = consumer.get_cl_twap(&cl_addr, &60_u64);
        assert_eq!(amm_alone, 1_000_000);
        assert_eq!(cl_alone, -250);

        // Both entries are present, correctly tagged, and each equals the
        // value its own single-pool query reports: the CL pool no longer
        // affects the AMM entry.
        let all = consumer.get_twap_all(&60_u64);
        assert_eq!(
            all,
            soroban_sdk::vec![
                &env,
                TwapEntry {
                    pool: amm.address.clone(),
                    pool_type: PoolType::Amm,
                    twap: amm_alone,
                },
                TwapEntry {
                    pool: cl_addr.clone(),
                    pool_type: PoolType::Cl,
                    twap: i128::from(cl_alone),
                },
            ]
        );
    }

    #[test]
    fn test_interface_mismatch_is_a_typed_error_not_a_trap() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);
        let admin = Address::generate(&env);

        let amm = seeded_amm(&env, &admin);
        let cl_addr = env.register_contract(None, MockClPool);
        let not_a_contract = Address::generate(&env);
        let consumer = new_consumer(&env, &admin);

        // Each path called against the wrong interface, or against no
        // contract at all, reports CrossContractCallFailed.
        let failed = Ok(TwapError::CrossContractCallFailed);
        assert_eq!(consumer.try_save_snapshot(&cl_addr).unwrap_err(), failed);
        assert_eq!(
            consumer.try_save_cl_snapshot(&amm.address).unwrap_err(),
            failed
        );
        assert_eq!(
            consumer.try_get_twap_price(&cl_addr, &60_u64).unwrap_err(),
            failed
        );
        assert_eq!(
            consumer.try_get_twap_both(&cl_addr, &60_u64).unwrap_err(),
            failed
        );
        assert_eq!(
            consumer.try_get_cl_twap(&amm.address, &60_u64).unwrap_err(),
            failed
        );
        assert_eq!(
            consumer
                .try_get_cl_twap(&not_a_contract, &60_u64)
                .unwrap_err(),
            failed
        );

        // A failed save registers nothing.
        assert_eq!(consumer.get_tracked_pools().len(), 0);
    }

    #[test]
    fn test_legacy_untyped_tracked_pools_are_migrated() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);
        let admin = Address::generate(&env);

        let amm = seeded_amm(&env, &admin);
        let cl_addr = env.register_contract(None, MockClPool);
        let cl = MockClPoolClient::new(&env, &cl_addr);
        let consumer = new_consumer(&env, &admin);

        // State as a pre-#964 contract left it: snapshots for both pools and
        // an untyped tracked list that includes the CL pool.
        consumer.save_snapshot(&amm.address);
        consumer.save_cl_snapshot(&cl_addr);
        env.as_contract(&consumer.address, || {
            let storage = env.storage().persistent();
            storage.remove(&DataKey::TrackedPools);
            let legacy = soroban_sdk::vec![&env, amm.address.clone(), cl_addr.clone()];
            storage.set(&DataKey::TrackedPoolsPersistent, &legacy);
        });

        // Legacy entries read as Amm, in their stored order.
        let typed = consumer.get_tracked_pools_typed();
        assert_eq!(typed.len(), 2);
        assert!(typed.iter().all(|t| t.pool_type == PoolType::Amm));
        assert_eq!(
            consumer.get_tracked_pools(),
            soroban_sdk::vec![&env, amm.address.clone(), cl_addr.clone()]
        );

        env.ledger().set_timestamp(10_060);
        poke_amm(&env, &amm);
        cl.set_tick_cumulative(&(1_000_i64 + 100 * 60), &10_060_u64);

        // The mis-typed CL entry is a typed error, not a trap.
        assert_eq!(
            consumer.try_get_twap_all(&60_u64),
            Err(Ok(TwapError::CrossContractCallFailed))
        );

        // The keeper's next CL snapshot re-types it and writes the typed list;
        // the legacy key is gone and order is preserved.
        consumer.save_cl_snapshot(&cl_addr);
        let typed = consumer.get_tracked_pools_typed();
        assert_eq!(typed.get(0).unwrap().address, amm.address);
        assert_eq!(typed.get(0).unwrap().pool_type, PoolType::Amm);
        assert_eq!(typed.get(1).unwrap().address, cl_addr);
        assert_eq!(typed.get(1).unwrap().pool_type, PoolType::Cl);
        env.as_contract(&consumer.address, || {
            assert!(!env
                .storage()
                .persistent()
                .has(&DataKey::TrackedPoolsPersistent));
        });

        let all = consumer.get_twap_all(&60_u64);
        assert_eq!(all.len(), 2);
        assert_eq!(all.get(0).unwrap().twap, 1_000_000);
        assert_eq!(all.get(1).unwrap().pool_type, PoolType::Cl);
        assert_eq!(all.get(1).unwrap().twap, 100);
    }

    #[test]
    fn test_saving_again_keeps_one_entry_per_pool() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);
        let admin = Address::generate(&env);

        let amm = seeded_amm(&env, &admin);
        let cl_addr = env.register_contract(None, MockClPool);
        let consumer = new_consumer(&env, &admin);

        consumer.save_snapshot(&amm.address);
        consumer.save_cl_snapshot(&cl_addr);
        env.ledger().set_timestamp(10_060);
        consumer.save_snapshot(&amm.address);
        consumer.save_cl_snapshot(&cl_addr);

        let typed = consumer.get_tracked_pools_typed();
        assert_eq!(typed.len(), 2);
        assert_eq!(typed.get(0).unwrap().pool_type, PoolType::Amm);
        assert_eq!(typed.get(1).unwrap().pool_type, PoolType::Cl);
    }
}
