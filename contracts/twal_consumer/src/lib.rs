#![no_std]

//! TWAL (time-weighted average liquidity) consumer contract.
//!
//! Snapshot entries share a persistent-storage TTL of [`TwalConsumer::SNAPSHOT_TTL_LEDGERS`]
//! (about seven days at five seconds per ledger), while TWAL reads permit
//! windows up to [`TwalConsumer::MAX_WINDOW_SECONDS`] (90 days). A keeper must
//! continue saving snapshots to keep the history entry live; callers should
//! use `max_servable_window` to check how much retained history is available
//! before requesting a TWAL window.

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, Address, Env, Symbol, Vec,
};

#[contractclient(name = "AmmPoolLiquidityClient")]
pub trait AmmPoolLiquidityOracle {
    fn get_liquidity_cumulative(env: Env) -> (i128, u64);
    fn get_info(env: Env) -> AmmPoolInfo;
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

#[contractclient(name = "ClPoolLiquidityClient")]
pub trait ClPoolLiquidityOracle {
    fn active_liquidity(env: Env) -> i128;
    fn get_tick_cumulative(env: Env) -> (i64, u64);
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TwalError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    ZeroWindow = 3,
    InsufficientHistory = 4,
    NoSnapshotFound = 5,
    ElapsedZero = 6,
    /// `add_tracked_pool` (explicit or implicit, via `save_snapshot`/
    /// `save_cl_snapshot`) would push the tracked set past
    /// `TwalConsumer::MAX_TRACKED_POOLS`.
    TooManyTrackedPools = 7,
    /// `remove_tracked_pool` was called with a pool that is not in the
    /// tracked set.
    NotTracked = 8,
    /// `window_seconds` exceeded `TwalConsumer::MAX_WINDOW_SECONDS`.
    WindowTooLarge = 9,
    /// `get_twal_batch` was called with more pools than
    /// `TwalConsumer::MAX_TRACKED_POOLS`.
    TooManyPools = 10,
    /// The cross-contract call into the pool's liquidity oracle failed —
    /// either it returned a host-level invocation error, or the callee
    /// panicked (e.g. the address is not a contract, or has a bug).
    CrossContractCallFailed = 11,
    /// A CL TWAL query found a stored `LiquiditySnapshot` for the pool but no
    /// `ClAccumulator` entry, so there is no running liquidity-cumulative to
    /// extrapolate to the current ledger time. Call `save_cl_snapshot` for the
    /// pool (which writes both) before querying `get_cl_twal`.
    MissingClAccumulator = 12,
    /// Reserve multiplication or cumulative extrapolation overflowed i128.
    ArithmeticOverflow = 13,
    /// `accept_keeper` was called without a pending keeper proposal.
    NoPendingKeeper = 14,
    /// `accept_keeper` was called by an address other than the pending keeper.
    WrongPendingKeeper = 15,
    /// Deleting this snapshot would remove the newest snapshot or leave fewer
    /// than two snapshots available for history reads.
    CannotDeleteRequiredSnapshot = 16,
}

#[contracttype]
pub enum DataKey {
    Keeper,
    TrackedPoolsPersistent,
    /// Every retained liquidity snapshot for a pool, in one entry, sorted by
    /// ascending (deduplicated) ledger timestamp. Readers binary-search it for
    /// the most recent snapshot at or before an arbitrary `then_ts` (issue
    /// #469).
    ///
    /// Keyed by pool only (issue #985). Snapshots used to live under
    /// `LiquiditySnapshot(pool, ledger_timestamp)`, but a Soroban
    /// transaction's footprint is fixed at simulation, against an earlier
    /// ledger with an earlier timestamp, so on a real network every save wrote
    /// a key outside its footprint and trapped. Reads had the same flaw: the
    /// snapshot they loaded was chosen by the current timestamp. No storage
    /// key in this contract may depend on the ledger timestamp or sequence;
    /// `scripts/check_storage_keys.sh` enforces that in CI.
    Snapshots(Address),
    /// Running liquidity-cumulative state for a CL pool (see `save_cl_snapshot`).
    ClAccumulator(Address),
    PendingKeeper,
}

/// One retained snapshot as stored under [`DataKey::Snapshots`]:
/// `(ledger_ts, cum_liquidity, pool_ts)`. A tuple rather than
/// [`LiquiditySnapshot`] because it encodes at roughly half the size, which
/// is what lets [`TwalConsumer::MAX_SNAPSHOTS_PER_POOL`] snapshots fit in a
/// single ledger entry.
type SnapshotEntry = (u64, i128, u64);

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PoolType {
    Amm,
    Cl,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackedPool {
    pub address: Address,
    pub pool_type: PoolType,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiquiditySnapshot {
    pub cum_liquidity: i128,
    pub pool_ts: u64,
}

/// One pool's result from a fault-isolated batch TWAL read.
///
/// `get_twal_all_safe` and `get_twal_batch` always return exactly one entry
/// per pool queried, whether or not that pool's read succeeded, so a single
/// dead or reverting pool can never take down the rest of the batch.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct TwalEntry {
    pub pool: Address,
    pub twal: i128,
    pub ok: bool,
    /// Populated when `ok == false`; the `TwalError` discriminant that
    /// caused this pool to be skipped. Meaningless (`0`) when `ok == true`.
    pub error_code: u32,
}

/// A TWAL value together with the ledger-time interval it actually covers.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TwalResult {
    pub twal: i128,
    pub window_used: u64,
}

/// Running integral of a CL pool's active liquidity over ledger time.
///
/// Concentrated-liquidity pools only expose the *instantaneous* active
/// liquidity, so — unlike the AMM path — there is no pool-side cumulative to
/// difference. This contract builds one itself: on each keeper snapshot it adds
/// `last_active * elapsed` to `cum_liquidity`, turning a series of instantaneous
/// readings into a proper time-weighted accumulator that `get_cl_twal` can
/// difference to recover an average liquidity *level* (issue #462).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClLiquidityAccumulator {
    /// Integral of active liquidity over ledger time (liquidity-seconds).
    pub cum_liquidity: i128,
    /// Ledger timestamp at which the accumulator was last updated.
    pub last_ts: u64,
    /// Active liquidity recorded at `last_ts`, held constant until the next update.
    pub last_active: i128,
}

#[contract]
pub struct TwalConsumer;

#[contractimpl]
impl TwalConsumer {
    pub const SNAPSHOT_TTL_LEDGERS: u32 = 120_960;

    /// Ceiling on the tracked-pool set. Keeper-only mutation plus this cap
    /// bounds `get_twal_all` / `get_twal_all_safe` cost so neither read path
    /// can be pushed past the transaction resource limit by an oversized
    /// tracked set.
    pub const MAX_TRACKED_POOLS: u32 = 100;

    /// Ceiling on `window_seconds` for any TWAL read (90 days). Snapshot
    /// storage has a roughly seven-day TTL, so this ceiling is only servable
    /// while the keeper continues refreshing the pool's snapshot entry.
    pub const MAX_WINDOW_SECONDS: u64 = 7_776_000;

    /// Ceiling on snapshots retained per pool. All of a pool's snapshots
    /// share one ledger entry, and ledger entries are size-limited (64 KiB on
    /// public networks); at about 56 bytes per snapshot this keeps a full
    /// entry near 28 KiB. Once reached, each save drops the oldest snapshot,
    /// so a keeper saving every `MAX_WINDOW_SECONDS / MAX_SNAPSHOTS_PER_POOL`
    /// (about 4.2 hours) or less often keeps the full 90-day window, and one
    /// saving every 3 minutes still keeps a full day.
    pub const MAX_SNAPSHOTS_PER_POOL: u32 = 512;

    /// Instance-storage TTL maintenance.
    ///
    /// `twal_consumer` keeps the keeper address in instance storage, so the
    /// instance entry holds the contract's executable together with its only
    /// instance datum. A sporadically-read oracle surface is the most likely
    /// contract in the workspace to have its instance entry lapse; if it does,
    /// every call — including the read paths integrators depend on — traps
    /// instead of returning a typed error. Every public entrypoint therefore
    /// bumps the instance TTL. The persistent snapshot entries keep their own,
    /// longer TTL bumps on the write paths (see `SNAPSHOT_TTL_LEDGERS`).
    ///
    /// Threshold and bump are expressed in ledgers. At ~5 s per ledger, 172_800
    /// ledgers is about 10 days (top up when less than this remains) and
    /// 518_400 ledgers is about 30 days (the target live-until). These match
    /// the `amm` reference contract so the workspace ages its instance entries
    /// on one schedule.
    fn extend_ttl(env: &Env) {
        env.storage().instance().extend_ttl(172_800, 518_400);
    }

    pub fn initialize(env: Env, keeper: Address) -> Result<(), TwalError> {
        Self::extend_ttl(&env);
        if env.storage().instance().has(&DataKey::Keeper) {
            return Err(TwalError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Keeper, &keeper);
        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "initialized"),),
            (keeper,)
        );
        Ok(())
    }

    /// Propose a keeper successor. Only the current keeper may nominate one;
    /// the nominee must separately authorize `accept_keeper`.
    pub fn propose_keeper(env: Env, new_keeper: Address) -> Result<(), TwalError> {
        Self::extend_ttl(&env);
        Self::require_keeper(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::PendingKeeper, &new_keeper);
        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "keeper_proposed"),),
            (new_keeper,)
        );
        Ok(())
    }

    /// Accept the pending keeper proposal. The pending address must authorize
    /// the call; strangers cannot accept a transfer on its behalf.
    pub fn accept_keeper(env: Env, new_keeper: Address) -> Result<(), TwalError> {
        Self::extend_ttl(&env);
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingKeeper)
            .ok_or(TwalError::NoPendingKeeper)?;
        if new_keeper != pending {
            return Err(TwalError::WrongPendingKeeper);
        }
        new_keeper.require_auth();
        env.storage().instance().set(&DataKey::Keeper, &new_keeper);
        env.storage().instance().remove(&DataKey::PendingKeeper);
        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "keeper_changed"),),
            (new_keeper,)
        );
        Ok(())
    }

    /// Return the pending keeper nominee, or `None` when no transfer is active.
    pub fn get_pending_keeper(env: Env) -> Option<Address> {
        Self::extend_ttl(&env);
        env.storage().instance().get(&DataKey::PendingKeeper)
    }

    /// Replace this contract's WASM. Keeper-only; contract state is preserved.
    pub fn upgrade(env: Env, new_wasm_hash: soroban_sdk::BytesN<32>) -> Result<(), TwalError> {
        Self::extend_ttl(&env);
        Self::require_keeper(&env)?;
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "upgraded"),),
            (new_wasm_hash,)
        );
        Ok(())
    }

    pub fn get_keeper(env: Env) -> Result<Address, TwalError> {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Keeper)
            .ok_or(TwalError::NotInitialized)
    }

    fn require_keeper(env: &Env) -> Result<(), TwalError> {
        Self::get_keeper(env.clone())?.require_auth();
        Ok(())
    }

    pub fn save_snapshot(env: Env, pool: Address) -> Result<(), TwalError> {
        Self::extend_ttl(&env);
        Self::require_keeper(&env)?;
        let (cum, pool_ts) = AmmPoolLiquidityClient::new(&env, &pool).get_liquidity_cumulative();
        Self::register_tracked_pool(&env, &pool, PoolType::Amm)?;
        Self::record_snapshot(&env, &pool, cum, pool_ts);
        Ok(())
    }

    fn load_tracked(env: &Env) -> Vec<TrackedPool> {
        env.storage()
            .persistent()
            .get(&DataKey::TrackedPoolsPersistent)
            .unwrap_or_else(|| Vec::new(env))
    }

    fn store_tracked(env: &Env, tracked: &Vec<TrackedPool>) {
        env.storage()
            .persistent()
            .set(&DataKey::TrackedPoolsPersistent, tracked);
        env.storage().persistent().extend_ttl(
            &DataKey::TrackedPoolsPersistent,
            Self::SNAPSHOT_TTL_LEDGERS / 2,
            Self::SNAPSHOT_TTL_LEDGERS,
        );
    }

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

    /// Record a snapshot at the current ledger time. Ledger timestamps never
    /// decrease, so appending keeps the list sorted; a second save within the
    /// same ledger replaces the first. Past [`Self::MAX_SNAPSHOTS_PER_POOL`]
    /// the oldest snapshot is dropped.
    ///
    /// Every key touched here is fixed given `pool`, so the footprint a
    /// client simulates is the footprint the transaction applies with, even
    /// though the ledger timestamp moves on in between (issue #985).
    fn record_snapshot(env: &Env, pool: &Address, cum_liquidity: i128, pool_ts: u64) {
        let ledger_ts = env.ledger().timestamp();
        let entry: SnapshotEntry = (ledger_ts, cum_liquidity, pool_ts);
        let mut snapshots = Self::load_snapshots(env, pool);
        match snapshots.last() {
            Some(last) if last.0 == ledger_ts => snapshots.set(snapshots.len() - 1, entry),
            _ => snapshots.push_back(entry),
        }
        while snapshots.len() > Self::MAX_SNAPSHOTS_PER_POOL {
            snapshots.pop_front();
        }
        Self::store_snapshots(env, pool, &snapshots);
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
            return None;
        }
        snapshots.get(lo - 1)
    }

    fn integer_sqrt(n: i128) -> Result<i128, TwalError> {
        if n < 0 {
            return Err(TwalError::ArithmeticOverflow);
        }
        if n == 0 {
            return Ok(0);
        }
        let mut x = n;
        let mut y = x / 2 + x % 2;
        while y < x {
            x = y;
            y = (x + n / x) / 2;
        }
        Ok(x)
    }

    /// Extrapolate the pool accumulator to ledger time using the same
    /// geometric-mean liquidity formula as `amm::checkpoint_twal`.
    fn get_amm_twal_checked(
        env: &Env,
        pool: &Address,
        window_seconds: u64,
    ) -> Result<TwalResult, TwalError> {
        if window_seconds == 0 {
            return Err(TwalError::ZeroWindow);
        }
        if window_seconds > Self::MAX_WINDOW_SECONDS {
            return Err(TwalError::WindowTooLarge);
        }
        let ledger_ts_now = env.ledger().timestamp();
        if ledger_ts_now < window_seconds {
            return Err(TwalError::InsufficientHistory);
        }

        let client = AmmPoolLiquidityClient::new(env, pool);
        let (cum_stored, pool_ts_now) = match client.try_get_liquidity_cumulative() {
            Ok(Ok(value)) => value,
            _ => return Err(TwalError::CrossContractCallFailed),
        };
        let info = match client.try_get_info() {
            Ok(Ok(value)) => value,
            _ => return Err(TwalError::CrossContractCallFailed),
        };
        let reserve_product = info
            .reserve_a
            .checked_mul(info.reserve_b)
            .ok_or(TwalError::ArithmeticOverflow)?;
        let liquidity = Self::integer_sqrt(reserve_product)?;
        let idle_elapsed = ledger_ts_now.saturating_sub(pool_ts_now) as i128;
        let extrapolated = liquidity
            .checked_mul(idle_elapsed)
            .and_then(|delta| cum_stored.checked_add(delta))
            .ok_or(TwalError::ArithmeticOverflow)?;

        let then_ts = ledger_ts_now - window_seconds;
        let (snapshot_ts, snapshot_cumulative, _) =
            Self::floor_snapshot_entry(env, pool, then_ts).ok_or(TwalError::NoSnapshotFound)?;
        let window_used = ledger_ts_now.saturating_sub(snapshot_ts);
        if window_used == 0 {
            return Err(TwalError::ElapsedZero);
        }
        let delta = (extrapolated as u128).wrapping_sub(snapshot_cumulative as u128) as i128;
        Ok(TwalResult {
            twal: delta / window_used as i128,
            window_used,
        })
    }

    /// Returns the number of snapshots retained for `pool`.
    pub fn get_snapshot_count(env: Env, pool: Address) -> u32 {
        Self::extend_ttl(&env);
        Self::load_snapshots(&env, &pool).len()
    }

    /// Return the age in seconds of the oldest retained snapshot, or zero if
    /// the pool has no retained snapshots. This is the longest history window
    /// currently supported by stored data, independent of pool type.
    pub fn max_servable_window(env: Env, pool: Address) -> u64 {
        Self::extend_ttl(&env);
        let snapshots = Self::load_snapshots(&env, &pool);
        match snapshots.get(0) {
            Some(oldest) => env.ledger().timestamp().saturating_sub(oldest.0),
            None => 0,
        }
    }

    fn first_tracked_index(tracked: &Vec<TrackedPool>, pool: &Address) -> Option<u32> {
        (0..tracked.len()).find(|&i| tracked.get(i).unwrap().address == *pool)
    }

    /// Shared implementation behind both the explicit `add_tracked_pool` and
    /// the implicit registration inside `save_snapshot` / `save_cl_snapshot`.
    /// Idempotent: adding an already-tracked pool is a no-op `Ok(())` and
    /// emits no event. Rejects growth past `MAX_TRACKED_POOLS`.
    fn register_tracked_pool(
        env: &Env,
        pool: &Address,
        pool_type: PoolType,
    ) -> Result<(), TwalError> {
        let mut tracked = Self::load_tracked(env);
        if Self::first_tracked_index(&tracked, pool).is_some() {
            return Ok(());
        }
        if tracked.len() >= Self::MAX_TRACKED_POOLS {
            return Err(TwalError::TooManyTrackedPools);
        }
        tracked.push_back(TrackedPool {
            address: pool.clone(),
            pool_type,
        });
        Self::store_tracked(env, &tracked);
        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(env, "pool_add"),), pool.clone());
        Ok(())
    }

    /// Adds `pool` to the tracked set as the given `pool_type`. Keeper-only,
    /// idempotent, rejects growth past `MAX_TRACKED_POOLS`.
    pub fn add_tracked_pool(env: Env, pool: Address, pool_type: PoolType) -> Result<(), TwalError> {
        Self::extend_ttl(&env);
        Self::require_keeper(&env)?;
        Self::register_tracked_pool(&env, &pool, pool_type)
    }

    /// Removes `pool` from the tracked set, preserving the relative order of
    /// the remaining pools. Keeper-only. Errors with `NotTracked` if the pool
    /// was never in the set.
    pub fn remove_tracked_pool(env: Env, pool: Address) -> Result<(), TwalError> {
        Self::extend_ttl(&env);
        Self::require_keeper(&env)?;
        let mut tracked = Self::load_tracked(&env);
        let idx = Self::first_tracked_index(&tracked, &pool).ok_or(TwalError::NotTracked)?;
        tracked.remove(idx);
        Self::store_tracked(&env, &tracked);
        soroban_amm_sdk::emit_versioned_event!(&env, (Symbol::new(&env, "pool_remove"),), pool);
        Ok(())
    }

    /// Returns whether `pool` is currently in the tracked set.
    pub fn is_tracked(env: Env, pool: Address) -> bool {
        Self::extend_ttl(&env);
        Self::first_tracked_index(&Self::load_tracked(&env), &pool).is_some()
    }

    /// Returns the number of currently tracked pools.
    pub fn get_tracked_pool_count(env: Env) -> u32 {
        Self::extend_ttl(&env);
        Self::load_tracked(&env).len()
    }

    /// Returns up to `limit` tracked pool addresses starting at `offset`.
    /// `limit == 0` and `offset >= count` both return an empty `Vec`.
    pub fn get_tracked_pools_paginated(env: Env, offset: u32, limit: u32) -> Vec<Address> {
        Self::extend_ttl(&env);
        let tracked = Self::load_tracked(&env);
        let len = tracked.len();
        let mut out = Vec::new(&env);
        if limit == 0 || offset >= len {
            return out;
        }
        let end = core::cmp::min(offset.saturating_add(limit), len);
        let mut i = offset;
        while i < end {
            out.push_back(tracked.get(i).unwrap().address);
            i += 1;
        }
        out
    }

    /// Shared implementation behind both the public `get_cl_twal` and the
    /// fault-isolated batch-read path (`compute_twal_entry`), so the two can
    /// never drift apart on which condition maps to which `TwalError`. Unlike
    /// the AMM path, a CL pool's TWAL query never makes a cross-contract call
    /// — all the data it needs was already captured by `save_cl_snapshot` —
    /// so the only failure modes are the typed ones below, not a panicking
    /// callee.
    fn get_cl_twal_checked(
        env: &Env,
        pool: &Address,
        window_seconds: u64,
    ) -> Result<i128, TwalError> {
        if window_seconds == 0 {
            return Err(TwalError::ZeroWindow);
        }
        if window_seconds > Self::MAX_WINDOW_SECONDS {
            return Err(TwalError::WindowTooLarge);
        }
        let ledger_ts_now = env.ledger().timestamp();
        if ledger_ts_now < window_seconds {
            return Err(TwalError::InsufficientHistory);
        }
        let then_ts = ledger_ts_now - window_seconds;
        // No snapshot at or before the window start: the pool's recorded
        // history does not reach back far enough, mirroring
        // `get_twal_liquidity`'s treatment of the same condition.
        let (snapshot_ts, snapshot_cumulative, _) =
            Self::floor_snapshot_entry(env, pool, then_ts).ok_or(TwalError::InsufficientHistory)?;
        let accumulator: ClLiquidityAccumulator = env
            .storage()
            .persistent()
            .get(&DataKey::ClAccumulator(pool.clone()))
            .ok_or(TwalError::MissingClAccumulator)?;

        let elapsed_since_update = ledger_ts_now.saturating_sub(accumulator.last_ts) as i128;
        let cum_now = accumulator.cum_liquidity + accumulator.last_active * elapsed_since_update;

        let elapsed = ledger_ts_now.saturating_sub(snapshot_ts) as i128;
        if elapsed <= 0 {
            return Err(TwalError::ElapsedZero);
        }
        Ok((cum_now - snapshot_cumulative) / elapsed)
    }

    /// Computes a single pool's TWAL entry without ever panicking or
    /// propagating a typed error — used by both `get_twal_all_safe` and
    /// `get_twal_batch` so a single bad pool can never abort a batch read.
    /// For an AMM pool the cross-contract call uses `try_invoke_contract`
    /// (via the generated `try_*` client method) so a panicking or
    /// non-contract callee is caught here too, not just a typed `Err`; a CL
    /// pool's query never makes a cross-contract call at all (see
    /// `get_cl_twal_checked`), so only the typed failures apply there.
    fn compute_twal_entry(
        env: &Env,
        pool: Address,
        pool_type: PoolType,
        window_seconds: u64,
    ) -> TwalEntry {
        let fail = |code: TwalError| TwalEntry {
            pool: pool.clone(),
            twal: 0,
            ok: false,
            error_code: code as u32,
        };

        if window_seconds == 0 {
            return fail(TwalError::ZeroWindow);
        }
        if window_seconds > Self::MAX_WINDOW_SECONDS {
            return fail(TwalError::WindowTooLarge);
        }

        match pool_type {
            PoolType::Cl => match Self::get_cl_twal_checked(env, &pool, window_seconds) {
                Ok(twal) => TwalEntry {
                    pool,
                    twal,
                    ok: true,
                    error_code: 0,
                },
                Err(e) => fail(e),
            },
            PoolType::Amm => match Self::get_amm_twal_checked(env, &pool, window_seconds) {
                Ok(result) => TwalEntry {
                    pool,
                    twal: result.twal,
                    ok: true,
                    error_code: 0,
                },
                Err(e) => fail(e),
            },
        }
    }

    fn error_from_code(code: u32) -> TwalError {
        match code {
            3 => TwalError::ZeroWindow,
            4 => TwalError::InsufficientHistory,
            5 => TwalError::NoSnapshotFound,
            6 => TwalError::ElapsedZero,
            9 => TwalError::WindowTooLarge,
            12 => TwalError::MissingClAccumulator,
            13 => TwalError::ArithmeticOverflow,
            _ => TwalError::CrossContractCallFailed,
        }
    }

    pub fn get_twal_liquidity(
        env: Env,
        pool: Address,
        window_seconds: u64,
    ) -> Result<i128, TwalError> {
        Self::extend_ttl(&env);
        Self::get_amm_twal_checked(&env, &pool, window_seconds)
            .map(|result| result.twal)
            .map_err(|error| match error {
                TwalError::NoSnapshotFound => TwalError::InsufficientHistory,
                other => other,
            })
    }

    /// Return AMM TWAL and the effective ledger-time window selected by the
    /// nearest retained snapshot at or before the requested boundary.
    pub fn get_twal_liquidity_detailed(
        env: Env,
        pool: Address,
        window_seconds: u64,
    ) -> Result<TwalResult, TwalError> {
        Self::extend_ttl(&env);
        Self::get_amm_twal_checked(&env, &pool, window_seconds).map_err(|error| match error {
            TwalError::NoSnapshotFound => TwalError::InsufficientHistory,
            other => other,
        })
    }

    /// Fault-isolated batch read over every currently tracked pool. Returns
    /// exactly one `TwalEntry` per tracked pool, always — a pool with no
    /// usable snapshot, a non-contract address, or a panicking callee yields
    /// `ok: false` in its slot rather than aborting the call.
    pub fn get_twal_all_safe(env: Env, window_seconds: u64) -> Vec<TwalEntry> {
        Self::extend_ttl(&env);
        let tracked = Self::load_tracked(&env);
        let mut out = Vec::new(&env);
        for i in 0..tracked.len() {
            let tracked_pool = tracked.get(i).unwrap();
            out.push_back(Self::compute_twal_entry(
                &env,
                tracked_pool.address,
                tracked_pool.pool_type,
                window_seconds,
            ));
        }
        out
    }

    /// Fault-isolated batch read over a caller-supplied subset of pools, so
    /// a consumer can read the pools it cares about without depending on the
    /// global tracked set. Each pool's type must be supplied explicitly,
    /// since an untracked, caller-chosen address has no recorded type to
    /// look up. Capped at `MAX_TRACKED_POOLS` entries.
    pub fn get_twal_batch(
        env: Env,
        pools: Vec<(Address, PoolType)>,
        window_seconds: u64,
    ) -> Result<Vec<TwalEntry>, TwalError> {
        Self::extend_ttl(&env);
        if pools.len() > Self::MAX_TRACKED_POOLS {
            return Err(TwalError::TooManyPools);
        }
        let mut out = Vec::new(&env);
        for i in 0..pools.len() {
            let (pool, pool_type) = pools.get(i).unwrap();
            out.push_back(Self::compute_twal_entry(
                &env,
                pool,
                pool_type,
                window_seconds,
            ));
        }
        Ok(out)
    }

    /// Batch TWAL read over every tracked pool (both AMM and CL). Kept on
    /// the ABI for existing callers: returns `Ok` with the same values as
    /// before when every pool succeeds, and the first `Err` encountered
    /// otherwise — identical external behaviour to the pre-#695
    /// implementation, now also covering CL pools. New callers should
    /// prefer `get_twal_all_safe` for per-pool detail; unlike this
    /// function, it never aborts the whole batch on one bad pool.
    pub fn get_twal_all(env: Env, window_seconds: u64) -> Result<Vec<(Address, i128)>, TwalError> {
        Self::extend_ttl(&env);
        let entries = Self::get_twal_all_safe(env.clone(), window_seconds);
        let mut results: Vec<(Address, i128)> = Vec::new(&env);
        for i in 0..entries.len() {
            let entry = entries.get(i).unwrap();
            if !entry.ok {
                return Err(Self::error_from_code(entry.error_code));
            }
            results.push_back((entry.pool, entry.twal));
        }
        Ok(results)
    }

    /// Returns the full tracked-pool set, including each pool's type.
    pub fn get_tracked_pools(env: Env) -> Vec<TrackedPool> {
        Self::extend_ttl(&env);
        Self::load_tracked(&env)
    }

    /// Save a CL pool snapshot, accumulating the integral of active liquidity.
    ///
    /// A CL pool only exposes its *instantaneous* active liquidity, so each call
    /// advances a running accumulator by `last_active * elapsed` (the liquidity
    /// recorded at the previous snapshot, held constant over the interval since).
    /// The resulting cumulative — not the raw instantaneous reading — is stored
    /// in the snapshot, so `get_cl_twal` can difference two snapshots to recover
    /// an average liquidity *level* rather than a rate of change (issue #462).
    pub fn save_cl_snapshot(env: Env, pool: Address) -> Result<(), TwalError> {
        Self::extend_ttl(&env);
        Self::require_keeper(&env)?;
        let active = ClPoolLiquidityClient::new(&env, &pool).active_liquidity();
        let ledger_ts = env.ledger().timestamp();

        let acc_key = DataKey::ClAccumulator(pool.clone());
        let cum = match env
            .storage()
            .persistent()
            .get::<_, ClLiquidityAccumulator>(&acc_key)
        {
            Some(prev) => {
                let elapsed = ledger_ts.saturating_sub(prev.last_ts) as i128;
                prev.cum_liquidity + prev.last_active * elapsed
            }
            // First snapshot for this pool: cumulative starts at zero.
            None => 0,
        };

        let accumulator = ClLiquidityAccumulator {
            cum_liquidity: cum,
            last_ts: ledger_ts,
            last_active: active,
        };
        env.storage().persistent().set(&acc_key, &accumulator);
        env.storage().persistent().extend_ttl(
            &acc_key,
            Self::SNAPSHOT_TTL_LEDGERS / 2,
            Self::SNAPSHOT_TTL_LEDGERS,
        );

        // Store the running cumulative (and the ledger time it corresponds to)
        // so a later query can difference two points of the integral.
        Self::register_tracked_pool(&env, &pool, PoolType::Cl)?;
        Self::record_snapshot(&env, &pool, cum, ledger_ts);
        Ok(())
    }

    /// Deletes a stored liquidity snapshot from persistent storage. Keeper-only.
    pub fn delete_snapshot(env: Env, pool: Address, ledger_ts: u64) -> Result<(), TwalError> {
        Self::extend_ttl(&env);
        Self::require_keeper(&env)?;
        let mut snapshots = Self::load_snapshots(&env, &pool);
        let index = snapshots
            .iter()
            .position(|entry| entry.0 == ledger_ts)
            .ok_or(TwalError::NoSnapshotFound)?;
        if index as u32 == snapshots.len() - 1 || snapshots.len() < 3 {
            return Err(TwalError::CannotDeleteRequiredSnapshot);
        }
        let removed = snapshots.get(index as u32).unwrap();
        snapshots.remove(index as u32);
        Self::store_snapshots(&env, &pool, &snapshots);
        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "snapshot_deleted"),),
            (pool, ledger_ts, removed.1)
        );
        Ok(())
    }
    /// Returns the time-weighted average active liquidity for a CL pool over
    /// `window_seconds`.
    ///
    /// Differences the liquidity-cumulative of the floor snapshot (the most
    /// recent one at or before `now - window`) against the accumulator
    /// extrapolated to the current ledger time, then divides by the elapsed
    /// span. A pool whose active liquidity is constant at `L` correctly reports
    /// `L`; the previous implementation differenced two *instantaneous* readings
    /// and so reported a rate of change (0 for constant liquidity) with the
    /// wrong units (issue #462).
    /// Returns a typed `TwalError` rather than panicking, matching
    /// `get_twal_liquidity`. A panic here would abort the whole calling
    /// transaction — including a composing contract such as
    /// `oracle_aggregator` — leaving it no way to tolerate a pool whose
    /// snapshots are missing or whose window is out of range (issue #792):
    ///
    /// - [`TwalError::ZeroWindow`] — `window_seconds == 0`.
    /// - [`TwalError::WindowTooLarge`] — `window_seconds` exceeds
    ///   [`TwalConsumer::MAX_WINDOW_SECONDS`].
    /// - [`TwalError::InsufficientHistory`] — the ledger clock predates the
    ///   window, or no snapshot exists at or before the window start.
    /// - [`TwalError::MissingClAccumulator`] — no `ClAccumulator` for the
    ///   pool; call `save_cl_snapshot` first.
    /// - [`TwalError::ElapsedZero`] — pool time did not advance across the
    ///   window.
    pub fn get_cl_twal(env: Env, pool: Address, window_seconds: u64) -> Result<i128, TwalError> {
        Self::extend_ttl(&env);
        Self::get_cl_twal_checked(&env, &pool, window_seconds)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use amm::{AmmPool, AmmPoolClient};
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::{StellarAssetClient, TokenClient as StellarTokenClient},
        Address, Env,
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

    #[test]
    fn test_twal_increases_with_liquidity_and_time() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwalConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "LP"),
            &soroban_sdk::String::from_str(&env, "LP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);
        AmmPoolClient::new(&env, &amm_addr).initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        AmmPoolClient::new(&env, &amm_addr).add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);
        consumer.save_snapshot(&amm_addr);

        env.ledger().with_mut(|l| l.timestamp = 10_600);
        ta_sac.mint(&provider, &200_000_i128);
        tb_sac.mint(&provider, &200_000_i128);
        AmmPoolClient::new(&env, &amm_addr).add_liquidity(
            &provider,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &u64::MAX,
        );
        consumer.save_snapshot(&amm_addr);

        env.ledger().with_mut(|l| l.timestamp = 11_200);
        let trader = Address::generate(&env);
        ta_sac.mint(&trader, &1_000_i128);
        AmmPoolClient::new(&env, &amm_addr).swap(
            &trader,
            &ta.address,
            &1_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let twal = consumer.get_twal_liquidity(&amm_addr, &600);
        assert!(twal > 0);
    }

    #[test]
    fn test_zero_window_returns_error() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let pool = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);

        assert_eq!(
            consumer.try_get_twal_liquidity(&pool, &0_u64),
            Err(Ok(TwalError::ZeroWindow))
        );
    }

    #[test]
    fn test_save_snapshot_requires_keeper_auth() {
        let env = Env::default();
        env.ledger().set_timestamp(10_000);

        let keeper = Address::generate(&env);
        let pool = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);

        assert!(consumer.try_save_snapshot(&pool).is_err());
        assert!(consumer.try_save_cl_snapshot(&pool).is_err());
    }

    #[test]
    fn test_save_snapshot_fails_when_uninitialized() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let pool = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);

        assert!(consumer.try_save_snapshot(&pool).is_err());
    }

    #[test]
    fn test_initialize_is_idempotent_guard() {
        let env = Env::default();
        env.mock_all_auths();

        let keeper = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);

        consumer.initialize(&keeper);
        assert_eq!(consumer.get_keeper(), keeper);
        assert_eq!(
            consumer.try_initialize(&Address::generate(&env)),
            Err(Ok(TwalError::AlreadyInitialized))
        );
    }

    #[test]
    fn test_delete_snapshot_removes_liquidity_snapshot() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwalConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "LP"),
            &soroban_sdk::String::from_str(&env, "LP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);
        AmmPoolClient::new(&env, &amm_addr).initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        AmmPoolClient::new(&env, &amm_addr).add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);
        consumer.save_snapshot(&amm_addr);
        env.ledger().with_mut(|l| l.timestamp = 10_100);
        consumer.save_snapshot(&amm_addr);
        env.ledger().with_mut(|l| l.timestamp = 10_200);
        consumer.save_snapshot(&amm_addr);

        consumer.delete_snapshot(&amm_addr, &10_100_u64);
        assert_eq!(consumer.get_snapshot_count(&amm_addr), 2);
    }

    // ── Issue #469: arbitrary window_seconds should hit the nearest older
    // snapshot instead of requiring an exact-timestamp match ──────────────────

    #[test]
    fn test_get_twal_liquidity_with_arbitrary_window_uses_floor_snapshot() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwalConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "LP"),
            &soroban_sdk::String::from_str(&env, "LP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);
        AmmPoolClient::new(&env, &amm_addr).initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        AmmPoolClient::new(&env, &amm_addr).add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);
        consumer.save_snapshot(&amm_addr);

        // A second snapshot 30s later, after more liquidity is added.
        env.ledger().with_mut(|l| l.timestamp = 10_030);
        ta_sac.mint(&provider, &100_000_i128);
        tb_sac.mint(&provider, &100_000_i128);
        AmmPoolClient::new(&env, &amm_addr).add_liquidity(
            &provider,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &u64::MAX,
        );
        consumer.save_snapshot(&amm_addr);

        // window=50 at ts=10_075 asks for then_ts=10_025, which has no exact
        // snapshot. Previously this reverted with NoSnapshotFound; now it
        // must fall back to the floor (the ts=10_000 snapshot).
        env.ledger().with_mut(|l| l.timestamp = 10_075);
        let twal = consumer.get_twal_liquidity(&amm_addr, &50);
        assert!(twal > 0);
        let detailed = consumer.get_twal_liquidity_detailed(&amm_addr, &50);
        assert_eq!(detailed.twal, twal);
        assert_eq!(detailed.window_used, 75);
    }

    #[test]
    fn test_delete_snapshot_requires_keeper_auth() {
        let env = Env::default();
        env.ledger().set_timestamp(10_000);

        let keeper = Address::generate(&env);
        let pool = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);

        assert!(consumer.try_delete_snapshot(&pool, &10_000_u64).is_err());
    }

    // ---- #695: tracked-pool lifecycle & fault-isolated batch reads ----

    /// A stub pool contract whose liquidity oracle always panics. Used to
    /// prove that batch reads catch a genuinely trapping callee, not only a
    /// typed `TwalError`.
    #[contract]
    pub struct PanicPool;

    #[contractimpl]
    impl PanicPool {
        pub fn get_liquidity_cumulative(_env: Env) -> (i128, u64) {
            panic!("PanicPool always panics");
        }
    }

    fn setup_consumer(env: &Env) -> (Address, TwalConsumerClient<'_>) {
        let keeper = Address::generate(env);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(env, &consumer_addr);
        consumer.initialize(&keeper);
        (keeper, consumer)
    }

    #[test]
    fn test_add_tracked_pool_is_idempotent() {
        let env = Env::default();
        env.mock_all_auths();
        let (_keeper, consumer) = setup_consumer(&env);
        let pool = Address::generate(&env);

        assert!(!consumer.is_tracked(&pool));
        consumer.add_tracked_pool(&pool, &PoolType::Amm);
        assert!(consumer.is_tracked(&pool));
        assert_eq!(consumer.get_tracked_pool_count(), 1);

        // Adding again is a no-op, not an error, and count does not grow.
        consumer.add_tracked_pool(&pool, &PoolType::Amm);
        assert_eq!(consumer.get_tracked_pool_count(), 1);
    }

    #[test]
    fn test_add_tracked_pool_rejects_non_keeper() {
        let env = Env::default();
        let (_keeper, consumer) = setup_consumer(&env);
        let pool = Address::generate(&env);

        // No auths mocked: the keeper's require_auth() must reject this.
        assert!(consumer
            .try_add_tracked_pool(&pool, &PoolType::Amm)
            .is_err());
    }

    #[test]
    fn test_remove_tracked_pool_rejects_non_keeper() {
        let env = Env::default();
        env.mock_all_auths();
        let (_keeper, consumer) = setup_consumer(&env);
        let pool = Address::generate(&env);
        consumer.add_tracked_pool(&pool, &PoolType::Amm);
        env.set_auths(&[]);

        assert!(consumer.try_remove_tracked_pool(&pool).is_err());
    }

    #[test]
    fn test_add_tracked_pool_rejects_beyond_max() {
        let env = Env::default();
        env.mock_all_auths();
        let (_keeper, consumer) = setup_consumer(&env);

        for _ in 0..TwalConsumer::MAX_TRACKED_POOLS {
            consumer.add_tracked_pool(&Address::generate(&env), &PoolType::Amm);
        }
        assert_eq!(
            consumer.get_tracked_pool_count(),
            TwalConsumer::MAX_TRACKED_POOLS
        );

        let overflow_pool = Address::generate(&env);
        assert_eq!(
            consumer.try_add_tracked_pool(&overflow_pool, &PoolType::Amm),
            Err(Ok(TwalError::TooManyTrackedPools))
        );
        assert!(!consumer.is_tracked(&overflow_pool));
    }

    #[test]
    fn test_remove_tracked_pool_preserves_order_of_rest() {
        let env = Env::default();
        env.mock_all_auths();
        let (_keeper, consumer) = setup_consumer(&env);

        let pools = [
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
        ];
        for p in pools.iter() {
            consumer.add_tracked_pool(p, &PoolType::Amm);
        }

        // Remove the middle pool; the rest must keep their relative order.
        consumer.remove_tracked_pool(&pools[2]);

        let remaining = consumer.get_tracked_pools_paginated(&0, &10);
        assert_eq!(remaining.len(), 4);
        assert_eq!(remaining.get(0).unwrap(), pools[0]);
        assert_eq!(remaining.get(1).unwrap(), pools[1]);
        assert_eq!(remaining.get(2).unwrap(), pools[3]);
        assert_eq!(remaining.get(3).unwrap(), pools[4]);
        assert!(!consumer.is_tracked(&pools[2]));
    }

    #[test]
    fn test_remove_tracked_pool_errors_when_not_tracked() {
        let env = Env::default();
        env.mock_all_auths();
        let (_keeper, consumer) = setup_consumer(&env);
        let pool = Address::generate(&env);

        assert_eq!(
            consumer.try_remove_tracked_pool(&pool),
            Err(Ok(TwalError::NotTracked))
        );
    }

    #[test]
    fn test_pagination_offset_equals_count_and_limit_zero() {
        let env = Env::default();
        env.mock_all_auths();
        let (_keeper, consumer) = setup_consumer(&env);
        let pool = Address::generate(&env);
        consumer.add_tracked_pool(&pool, &PoolType::Amm);

        assert_eq!(consumer.get_tracked_pools_paginated(&1, &10).len(), 0);
        assert_eq!(consumer.get_tracked_pools_paginated(&0, &0).len(), 0);
        assert_eq!(consumer.get_tracked_pools_paginated(&0, &10).len(), 1);
    }

    #[test]
    fn test_pagination_on_empty_set() {
        let env = Env::default();
        let (_keeper, consumer) = setup_consumer(&env);

        assert_eq!(consumer.get_tracked_pools_paginated(&0, &10).len(), 0);
        assert_eq!(consumer.get_tracked_pool_count(), 0);
    }

    #[test]
    fn test_implicit_registration_via_save_snapshot_respects_cap() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "LP"),
            &soroban_sdk::String::from_str(&env, "LP"),
            &7u32,
        );
        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);
        AmmPoolClient::new(&env, &amm_addr).initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );
        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        AmmPoolClient::new(&env, &amm_addr).add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let (_keeper, consumer) = setup_consumer(&env);
        for _ in 0..TwalConsumer::MAX_TRACKED_POOLS {
            consumer.add_tracked_pool(&Address::generate(&env), &PoolType::Amm);
        }

        // The pool itself is not yet tracked, so implicit registration inside
        // save_snapshot is what hits the cap.
        assert!(!consumer.is_tracked(&amm_addr));
        assert_eq!(
            consumer.try_save_snapshot(&amm_addr),
            Err(Ok(TwalError::TooManyTrackedPools))
        );
    }

    #[test]
    fn test_window_too_large_rejected_single_and_batch() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000_000);
        let (_keeper, consumer) = setup_consumer(&env);
        let pool = Address::generate(&env);

        let too_big = TwalConsumer::MAX_WINDOW_SECONDS + 1;
        assert_eq!(
            consumer.try_get_twal_liquidity(&pool, &too_big),
            Err(Ok(TwalError::WindowTooLarge))
        );

        let entries = consumer.get_twal_all_safe(&too_big);
        assert_eq!(entries.len(), 0); // pool not tracked, but no panic either

        consumer.add_tracked_pool(&pool, &PoolType::Amm);
        let entries = consumer.get_twal_all_safe(&too_big);
        assert_eq!(entries.len(), 1);
        let entry = entries.get(0).unwrap();
        assert!(!entry.ok);
        assert_eq!(entry.error_code, TwalError::WindowTooLarge as u32);
    }

    #[test]
    fn test_get_twal_all_safe_isolates_no_snapshot_pool() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);
        let (_keeper, consumer) = setup_consumer(&env);

        // Register a real AMM pool as tracked, but never snapshot it.
        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "LP"),
            &soroban_sdk::String::from_str(&env, "LP"),
            &7u32,
        );
        let (ta, _ta_sac) = create_sac(&env, &admin);
        let (tb, _tb_sac) = create_sac(&env, &admin);
        AmmPoolClient::new(&env, &amm_addr).initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );
        consumer.add_tracked_pool(&amm_addr, &PoolType::Amm);

        env.ledger().with_mut(|l| l.timestamp = 10_600);
        let entries = consumer.get_twal_all_safe(&600);
        assert_eq!(entries.len(), 1);
        let entry = entries.get(0).unwrap();
        assert!(!entry.ok);
        assert_eq!(entry.error_code, TwalError::NoSnapshotFound as u32);
        assert_eq!(entry.pool, amm_addr);
    }

    #[test]
    fn test_get_twal_all_safe_isolates_panicking_pool_not_transaction_failure() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);
        let (_keeper, consumer) = setup_consumer(&env);

        let panic_pool = env.register_contract(None, PanicPool);
        consumer.add_tracked_pool(&panic_pool, &PoolType::Amm);

        // Must not panic/abort the call despite the tracked pool's oracle
        // call always panicking.
        let entries = consumer.get_twal_all_safe(&600);
        assert_eq!(entries.len(), 1);
        let entry = entries.get(0).unwrap();
        assert!(!entry.ok);
        assert_eq!(entry.error_code, TwalError::CrossContractCallFailed as u32);
        assert_eq!(entry.twal, 0);
    }

    #[test]
    fn test_get_twal_all_safe_isolates_non_contract_address() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);
        let (_keeper, consumer) = setup_consumer(&env);

        // An address that was never registered as a contract at all.
        let not_a_contract = Address::generate(&env);
        consumer.add_tracked_pool(&not_a_contract, &PoolType::Amm);

        let entries = consumer.get_twal_all_safe(&600);
        assert_eq!(entries.len(), 1);
        let entry = entries.get(0).unwrap();
        assert!(!entry.ok);
        assert_eq!(entry.error_code, TwalError::CrossContractCallFailed as u32);
    }

    #[test]
    fn test_get_twal_all_still_errs_when_any_pool_fails() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);
        let (_keeper, consumer) = setup_consumer(&env);

        let panic_pool = env.register_contract(None, PanicPool);
        consumer.add_tracked_pool(&panic_pool, &PoolType::Amm);

        assert!(consumer.try_get_twal_all(&600).is_err());
    }

    #[test]
    fn test_get_twal_all_ok_matches_get_twal_liquidity_when_all_succeed() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "LP"),
            &soroban_sdk::String::from_str(&env, "LP"),
            &7u32,
        );
        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);
        AmmPoolClient::new(&env, &amm_addr).initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );
        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        AmmPoolClient::new(&env, &amm_addr).add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let (_keeper, consumer) = setup_consumer(&env);
        consumer.save_snapshot(&amm_addr);

        env.ledger().with_mut(|l| l.timestamp = 10_600);
        ta_sac.mint(&provider, &200_000_i128);
        tb_sac.mint(&provider, &200_000_i128);
        AmmPoolClient::new(&env, &amm_addr).add_liquidity(
            &provider,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &u64::MAX,
        );
        consumer.save_snapshot(&amm_addr);

        env.ledger().with_mut(|l| l.timestamp = 11_200);
        // Advance the pool's internal timestamp past the last snapshot so
        // `elapsed > 0`, matching the pattern used elsewhere in this file.
        let trader = Address::generate(&env);
        ta_sac.mint(&trader, &1_000_i128);
        AmmPoolClient::new(&env, &amm_addr).swap(
            &trader,
            &ta.address,
            &1_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let expected = consumer.get_twal_liquidity(&amm_addr, &600);
        let all = consumer.get_twal_all(&600);
        assert_eq!(all.len(), 1);
        assert_eq!(all.get(0).unwrap(), (amm_addr, expected));
    }

    #[test]
    fn test_get_twal_batch_reads_caller_supplied_subset() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);
        let (_keeper, consumer) = setup_consumer(&env);

        // Not part of the global tracked set at all.
        let panic_pool = env.register_contract(None, PanicPool);
        let not_a_contract = Address::generate(&env);
        let pools: Vec<(Address, PoolType)> = Vec::from_array(
            &env,
            [
                (panic_pool.clone(), PoolType::Amm),
                (not_a_contract.clone(), PoolType::Amm),
            ],
        );

        assert_eq!(consumer.get_tracked_pool_count(), 0);
        let entries = consumer.get_twal_batch(&pools, &600);
        assert_eq!(entries.len(), 2);
        assert!(!entries.get(0).unwrap().ok);
        assert!(!entries.get(1).unwrap().ok);
    }

    #[test]
    fn test_get_twal_batch_rejects_oversized_input() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);
        let (_keeper, consumer) = setup_consumer(&env);

        let mut pools: Vec<(Address, PoolType)> = Vec::new(&env);
        for _ in 0..(TwalConsumer::MAX_TRACKED_POOLS + 1) {
            pools.push_back((Address::generate(&env), PoolType::Amm));
        }

        assert_eq!(
            consumer.try_get_twal_batch(&pools, &600),
            Err(Ok(TwalError::TooManyPools))
        );
    }

    // Minimal CL pool stand-in exposing a settable instantaneous active
    // liquidity, matching the `ClPoolLiquidityOracle::active_liquidity` method
    // the consumer reads.
    #[contract]
    pub struct MockClPool;

    #[contractimpl]
    impl MockClPool {
        pub fn set_liquidity(env: Env, value: i128) {
            env.storage()
                .instance()
                .set(&soroban_sdk::symbol_short!("liq"), &value);
        }

        pub fn active_liquidity(env: Env) -> i128 {
            env.storage()
                .instance()
                .get(&soroban_sdk::symbol_short!("liq"))
                .unwrap_or(0)
        }
    }

    #[test]
    fn test_cl_twal_constant_liquidity_reports_level() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let keeper = Address::generate(&env);
        let pool_addr = env.register_contract(None, MockClPool);
        let pool = MockClPoolClient::new(&env, &pool_addr);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);

        // Active liquidity is constant at 5_000 across the whole window.
        pool.set_liquidity(&5_000_i128);
        consumer.save_cl_snapshot(&pool_addr); // baseline accumulator at t=10_000

        env.ledger().with_mut(|l| l.timestamp = 10_600);
        consumer.save_cl_snapshot(&pool_addr); // accumulator advances by 5_000*600

        env.ledger().with_mut(|l| l.timestamp = 11_200);
        let twal = consumer.get_cl_twal(&pool_addr, &600);

        // Constant liquidity must report its level, not a rate of change (0).
        assert_eq!(twal, 5_000);
    }

    #[test]
    fn test_cl_twal_is_time_weighted_average_of_levels() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let keeper = Address::generate(&env);
        let pool_addr = env.register_contract(None, MockClPool);
        let pool = MockClPoolClient::new(&env, &pool_addr);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);

        // 1_000 for the first 300s, then 3_000 for the next 300s.
        pool.set_liquidity(&1_000_i128);
        consumer.save_cl_snapshot(&pool_addr);

        env.ledger().with_mut(|l| l.timestamp = 10_300);
        pool.set_liquidity(&3_000_i128);
        consumer.save_cl_snapshot(&pool_addr);

        env.ledger().with_mut(|l| l.timestamp = 10_600);
        consumer.save_cl_snapshot(&pool_addr);

        // Window covers both segments: (1_000*300 + 3_000*300) / 600 = 2_000.
        let twal = consumer.get_cl_twal(&pool_addr, &600);
        assert_eq!(twal, 2_000);
    }

    // -- get_cl_twal typed errors (issue #792) --------------------------------

    /// A CL pool plus a consumer with a baseline snapshot already saved at
    /// `t = 10_000`, so tests only have to disturb the one condition they
    /// mean to exercise.
    fn setup_cl_pool(env: &Env) -> (Address, TwalConsumerClient<'_>) {
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let keeper = Address::generate(env);
        let pool_addr = env.register_contract(None, MockClPool);
        MockClPoolClient::new(env, &pool_addr).set_liquidity(&5_000_i128);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(env, &consumer_addr);
        consumer.initialize(&keeper);
        consumer.save_cl_snapshot(&pool_addr);
        (pool_addr, consumer)
    }

    #[test]
    fn test_cl_twal_zero_window_returns_typed_error() {
        let env = Env::default();
        let (pool_addr, consumer) = setup_cl_pool(&env);

        assert_eq!(
            consumer.try_get_cl_twal(&pool_addr, &0),
            Err(Ok(TwalError::ZeroWindow))
        );
    }

    #[test]
    fn test_cl_twal_window_too_large_returns_typed_error() {
        let env = Env::default();
        let (pool_addr, consumer) = setup_cl_pool(&env);
        // Push the clock past the window so the bound -- not the ledger age --
        // is what rejects the call.
        let too_big = TwalConsumer::MAX_WINDOW_SECONDS + 1;
        env.ledger().with_mut(|l| l.timestamp = too_big + 10_000);

        assert_eq!(
            consumer.try_get_cl_twal(&pool_addr, &too_big),
            Err(Ok(TwalError::WindowTooLarge))
        );
    }

    #[test]
    fn test_cl_twal_ledger_younger_than_window_returns_insufficient_history() {
        let env = Env::default();
        let (pool_addr, consumer) = setup_cl_pool(&env);
        // Ledger clock is at 10_000; a 20_000s window reaches before genesis.
        assert_eq!(
            consumer.try_get_cl_twal(&pool_addr, &20_000),
            Err(Ok(TwalError::InsufficientHistory))
        );
    }

    #[test]
    fn test_cl_twal_no_snapshot_before_window_returns_insufficient_history() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let keeper = Address::generate(&env);
        let pool_addr = env.register_contract(None, MockClPool);
        MockClPoolClient::new(&env, &pool_addr).set_liquidity(&5_000_i128);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);

        // The only snapshot is taken at 9_900, which is *after* the window
        // start of 10_000 - 600 = 9_400, so no floor snapshot exists.
        env.ledger().with_mut(|l| l.timestamp = 9_900);
        consumer.save_cl_snapshot(&pool_addr);
        env.ledger().with_mut(|l| l.timestamp = 10_000);

        assert_eq!(
            consumer.try_get_cl_twal(&pool_addr, &600),
            Err(Ok(TwalError::InsufficientHistory))
        );
    }

    #[test]
    fn test_cl_twal_missing_snapshot_history_returns_insufficient_history() {
        let env = Env::default();
        let (pool_addr, consumer) = setup_cl_pool(&env);
        let consumer_addr = consumer.address.clone();

        env.ledger().with_mut(|l| l.timestamp = 10_600);

        // Snapshots and their timestamps now share one entry, so an index
        // naming a missing snapshot can no longer happen; losing the entry
        // just means there is no history to read.
        env.as_contract(&consumer_addr, || {
            env.storage()
                .persistent()
                .remove(&DataKey::Snapshots(pool_addr.clone()));
        });

        assert_eq!(
            consumer.try_get_cl_twal(&pool_addr, &600),
            Err(Ok(TwalError::InsufficientHistory))
        );
    }

    #[test]
    fn test_cl_twal_missing_accumulator_returns_typed_error() {
        let env = Env::default();
        let (pool_addr, consumer) = setup_cl_pool(&env);
        let consumer_addr = consumer.address.clone();

        env.ledger().with_mut(|l| l.timestamp = 10_600);

        // Snapshot and its index survive; only the running accumulator is gone.
        env.as_contract(&consumer_addr, || {
            env.storage()
                .persistent()
                .remove(&DataKey::ClAccumulator(pool_addr.clone()));
        });

        assert_eq!(
            consumer.try_get_cl_twal(&pool_addr, &600),
            Err(Ok(TwalError::MissingClAccumulator))
        );
    }

    #[test]
    fn test_cl_twal_uses_snapshot_ledger_time_not_pool_update_time() {
        let env = Env::default();
        let (pool_addr, consumer) = setup_cl_pool(&env);
        let consumer_addr = consumer.address.clone();

        env.ledger().with_mut(|l| l.timestamp = 10_600);

        // Changing only the historical pool-update timestamp must not change
        // the ledger-time denominator used by the average.
        env.as_contract(&consumer_addr, || {
            let key = DataKey::Snapshots(pool_addr.clone());
            let mut snapshots: Vec<SnapshotEntry> = env.storage().persistent().get(&key).unwrap();
            let (ledger_ts, cum_liquidity, _) = snapshots.get(0).unwrap();
            assert_eq!(ledger_ts, 10_000);
            snapshots.set(0, (ledger_ts, cum_liquidity, 10_600));
            env.storage().persistent().set(&key, &snapshots);
        });

        assert_eq!(consumer.try_get_cl_twal(&pool_addr, &600), Ok(Ok(5_000)));
    }

    #[test]
    fn test_cl_twal_success_path_still_returns_ok_value() {
        let env = Env::default();
        let (pool_addr, consumer) = setup_cl_pool(&env);

        env.ledger().with_mut(|l| l.timestamp = 10_600);
        consumer.save_cl_snapshot(&pool_addr);
        env.ledger().with_mut(|l| l.timestamp = 11_200);

        // Constant 5_000 liquidity: the Result wrapper does not change the
        // computed value.
        assert_eq!(
            consumer.try_get_cl_twal(&pool_addr, &600),
            Ok(Ok(5_000_i128))
        );
        assert_eq!(consumer.get_cl_twal(&pool_addr, &600), 5_000);
    }

    #[test]
    fn test_cl_twal_batch_read_agrees_with_direct_typed_error() {
        let env = Env::default();
        let (pool_addr, consumer) = setup_cl_pool(&env);
        let consumer_addr = consumer.address.clone();

        env.ledger().with_mut(|l| l.timestamp = 10_600);
        env.as_contract(&consumer_addr, || {
            env.storage()
                .persistent()
                .remove(&DataKey::ClAccumulator(pool_addr.clone()));
        });

        // The fault-isolated batch path and the direct accessor share one
        // implementation, so they must report the same code.
        let entries = consumer.get_twal_all_safe(&600);
        assert_eq!(entries.len(), 1);
        let entry = entries.get(0).unwrap();
        assert!(!entry.ok);
        assert_eq!(entry.error_code, TwalError::MissingClAccumulator as u32);
        assert_eq!(
            consumer.try_get_cl_twal(&pool_addr, &600),
            Err(Ok(TwalError::MissingClAccumulator))
        );
    }

    /// Stub AMM pool whose liquidity cumulative grows with ledger time. In
    /// its own module so its generated symbols don't collide with the other
    /// stub pools'.
    mod stub_amm_pool {
        use soroban_sdk::{contract, contractimpl, Env};

        #[contract]
        pub struct StubAmmPool;

        #[contractimpl]
        impl StubAmmPool {
            pub fn get_liquidity_cumulative(env: Env) -> (i128, u64) {
                let ts = env.ledger().timestamp();
                (ts as i128 * 1_000, ts)
            }
        }
    }
    use stub_amm_pool::StubAmmPool;

    /// A consumer plus a stub AMM pool, with the clock at 10_000.
    fn setup_amm_pool(env: &Env) -> (Address, TwalConsumerClient<'_>) {
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);
        let (_keeper, consumer) = setup_consumer(env);
        (env.register_contract(None, StubAmmPool), consumer)
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
        let (pool, consumer) = setup_amm_pool(&env);
        consumer.save_snapshot(&pool);
        let keys_before = contract_data_keys(&env);
        let start = env.ledger().timestamp();
        for dt in [5, 60, 90_000] {
            env.ledger().with_mut(|l| l.timestamp = start + dt);
            consumer.save_snapshot(&pool);
            assert_eq!(
                contract_data_keys(&env),
                keys_before,
                "save at +{dt}s wrote a new key"
            );
        }
    }

    #[test]
    fn save_cl_snapshot_keys_do_not_depend_on_ledger_time() {
        let env = Env::default();
        let (pool, consumer) = setup_cl_pool(&env);
        let keys_before = contract_data_keys(&env);
        let start = env.ledger().timestamp();
        for dt in [1, 600, 86_400] {
            env.ledger().with_mut(|l| l.timestamp = start + dt);
            consumer.save_cl_snapshot(&pool);
            assert_eq!(
                contract_data_keys(&env),
                keys_before,
                "save at +{dt}s wrote a new key"
            );
        }
    }

    #[test]
    fn full_history_is_capped_fits_one_entry_and_stays_within_budget() {
        let env = Env::default();
        env.budget().reset_unlimited();
        let (pool, consumer) = setup_amm_pool(&env);
        let cap = TwalConsumer::MAX_SNAPSHOTS_PER_POOL;
        let start = env.ledger().timestamp();
        for i in 0..cap + 25 {
            env.ledger()
                .with_mut(|l| l.timestamp = start + 60 * i as u64);
            consumer.save_snapshot(&pool);
        }
        assert_eq!(consumer.get_snapshot_count(&pool), cap);

        let (oldest, encoded) = env.as_contract(&consumer.address, || {
            let snapshots: Vec<SnapshotEntry> = env
                .storage()
                .persistent()
                .get(&DataKey::Snapshots(pool.clone()))
                .unwrap();
            use soroban_sdk::xdr::ToXdr;
            (snapshots.get(0).unwrap().0, snapshots.to_xdr(&env).len())
        });
        assert_eq!(oldest, start + 60 * 25);
        assert!(
            encoded < 48 * 1024,
            "full history encodes to {encoded} bytes"
        );

        env.budget().reset_default();
        env.ledger()
            .with_mut(|l| l.timestamp = start + 60 * (cap + 25) as u64);
        consumer.save_snapshot(&pool);
        assert_eq!(consumer.get_snapshot_count(&pool), cap);
    }

    // ── Issue #919: every twal_consumer event carries EVENT_SCHEMA_VERSION ────
    //
    // These publish sites used to call `env.events().publish(...)` directly, so
    // their payloads were not version-stamped and an indexer reading
    // `(version, ...rest)` would have decoded the first real field as the
    // version number. Each test below pins the stamp for one topic.

    /// Fetch the payload of the most recent event `contract` published under the
    /// single-symbol `topic`, decoded as a version-stamped `(u32, T)` pair.
    fn last_versioned_event<T>(env: &Env, contract: &Address, topic: &str) -> (u32, T)
    where
        T: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>,
    {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::IntoVal;

        let wanted: soroban_sdk::Vec<soroban_sdk::Val> = (Symbol::new(env, topic),).into_val(env);
        let evt = env
            .events()
            .all()
            .iter()
            .rfind(|e| &e.0 == contract && e.1 == wanted)
            .unwrap_or_else(|| panic!("no `{topic}` event found"));
        evt.2.into_val(env)
    }

    #[test]
    fn test_pool_add_emits_versioned_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (_keeper, consumer) = setup_consumer(&env);
        let pool = Address::generate(&env);

        consumer.add_tracked_pool(&pool, &PoolType::Amm);

        let (version, data): (u32, Address) =
            last_versioned_event(&env, &consumer.address, "pool_add");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, pool);
    }

    #[test]
    fn test_pool_remove_emits_versioned_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (_keeper, consumer) = setup_consumer(&env);
        let pool = Address::generate(&env);

        consumer.add_tracked_pool(&pool, &PoolType::Amm);
        consumer.remove_tracked_pool(&pool);

        let (version, data): (u32, Address) =
            last_versioned_event(&env, &consumer.address, "pool_remove");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, pool);
    }

    #[test]
    fn test_snapshot_deleted_emits_versioned_event() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);

        let admin = Address::generate(&env);
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        let consumer_addr = env.register_contract(None, TwalConsumer);

        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "LP"),
            &soroban_sdk::String::from_str(&env, "LP"),
            &7u32,
        );

        let (ta, ta_sac) = create_sac(&env, &admin);
        let (tb, tb_sac) = create_sac(&env, &admin);
        AmmPoolClient::new(&env, &amm_addr).initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &1_000_000_i128);
        tb_sac.mint(&provider, &1_000_000_i128);
        AmmPoolClient::new(&env, &amm_addr).add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);
        consumer.save_snapshot(&amm_addr);
        env.ledger().with_mut(|l| l.timestamp = 10_100);
        consumer.save_snapshot(&amm_addr);
        env.ledger().with_mut(|l| l.timestamp = 10_200);
        consumer.save_snapshot(&amm_addr);
        let removed_cumulative = env.as_contract(&consumer.address, || {
            TwalConsumer::load_snapshots(&env, &amm_addr)
                .get(1)
                .unwrap()
                .1
        });
        consumer.delete_snapshot(&amm_addr, &10_100_u64);

        let (version, data): (u32, (Address, u64, i128)) =
            last_versioned_event(&env, &consumer.address, "snapshot_deleted");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (amm_addr, 10_100_u64, removed_cumulative));
    }

    // ── #911: instance TTL is extended on every entrypoint ───────────────────

    /// Advancing the ledger far past the default instance-entry TTL and then
    /// calling entrypoints must keep the contract responsive rather than
    /// trapping on an archived instance entry. The keeper lives in instance
    /// storage, so an archived instance entry means every call — including the
    /// read paths integrators depend on — traps. Each entrypoint now bumps the
    /// instance TTL; this walks the ledger forward in steps smaller than the
    /// bump window, calling in between, and asserts the reads still return.
    /// Without the extension the first call after the advance would trap.
    #[test]
    fn instance_ttl_is_extended_on_access_across_ledger_advance() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| {
            l.sequence_number = 1_000;
            l.max_entry_ttl = 6_312_000;
        });

        let admin = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&admin);

        let pool = Address::generate(&env);

        // Step forward in increments smaller than the TTL bump (518_400),
        // calling an entrypoint each step. Each call re-bumps, so the next step
        // stays live rather than trapping on an evicted instance entry.
        for _ in 0..4 {
            env.ledger().with_mut(|l| l.sequence_number += 400_000);
            assert_eq!(consumer.get_keeper(), admin);
            assert_eq!(consumer.get_tracked_pool_count(), 0);
            assert!(!consumer.is_tracked(&pool));
        }
    }

    #[test]
    fn initialize_emits_keeper_in_versioned_event() {
        let env = Env::default();
        let keeper = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);

        // The payload is the one-element tuple `(keeper,)`.
        let (version, (data,)): (u32, (Address,)) =
            last_versioned_event(&env, &consumer_addr, "initialized");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(data, keeper);
    }

    #[test]
    fn propose_keeper_requires_current_keeper_auth() {
        let env = Env::default();
        let keeper = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);

        assert!(consumer
            .try_propose_keeper(&Address::generate(&env))
            .is_err());
    }

    #[test]
    fn pending_keeper_view_is_empty_until_proposed() {
        let env = Env::default();
        env.mock_all_auths();
        let keeper = Address::generate(&env);
        let nominee = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);

        assert_eq!(consumer.get_pending_keeper(), None);
        consumer.propose_keeper(&nominee);
        assert_eq!(consumer.get_pending_keeper(), Some(nominee));
    }

    #[test]
    fn accept_keeper_requires_an_existing_proposal() {
        let env = Env::default();
        let keeper = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);

        assert_eq!(
            consumer.try_accept_keeper(&keeper),
            Err(Ok(TwalError::NoPendingKeeper))
        );
    }

    #[test]
    fn stranger_cannot_accept_keeper_proposal() {
        let env = Env::default();
        env.mock_all_auths();
        let keeper = Address::generate(&env);
        let nominee = Address::generate(&env);
        let stranger = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);
        consumer.propose_keeper(&nominee);

        assert_eq!(
            consumer.try_accept_keeper(&stranger),
            Err(Ok(TwalError::WrongPendingKeeper))
        );
    }

    #[test]
    fn accepting_keeper_changes_keeper_and_clears_pending() {
        let env = Env::default();
        env.mock_all_auths();
        let keeper = Address::generate(&env);
        let nominee = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);
        consumer.propose_keeper(&nominee);
        consumer.accept_keeper(&nominee);

        assert_eq!(consumer.get_keeper(), nominee);
        assert_eq!(consumer.get_pending_keeper(), None);
    }

    #[test]
    fn keeper_rotation_emits_versioned_proposal_and_change_events() {
        let env = Env::default();
        env.mock_all_auths();
        let keeper = Address::generate(&env);
        let nominee = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);
        consumer.propose_keeper(&nominee);
        let (proposal_version, (proposed,)): (u32, (Address,)) =
            last_versioned_event(&env, &consumer_addr, "keeper_proposed");
        consumer.accept_keeper(&nominee);
        let (change_version, (changed,)): (u32, (Address,)) =
            last_versioned_event(&env, &consumer_addr, "keeper_changed");

        assert_eq!(proposal_version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(proposed, nominee);
        assert_eq!(change_version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(changed, nominee);
    }

    #[test]
    fn old_keeper_cannot_call_any_keeper_write_path_after_rotation() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);
        let keeper = Address::generate(&env);
        let nominee = Address::generate(&env);
        let pool = env.register_contract(None, StubAmmPool);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);
        consumer.propose_keeper(&nominee);
        consumer.accept_keeper(&nominee);
        env.set_auths(&[]);

        assert!(consumer.try_save_snapshot(&pool).is_err());
        assert!(consumer.try_save_cl_snapshot(&pool).is_err());
        assert!(consumer
            .try_add_tracked_pool(&pool, &PoolType::Amm)
            .is_err());
        assert!(consumer.try_remove_tracked_pool(&pool).is_err());
        assert!(consumer.try_delete_snapshot(&pool, &10_000).is_err());
    }

    #[test]
    fn new_keeper_can_use_each_keeper_write_path_after_rotation() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);
        let keeper = Address::generate(&env);
        let nominee = Address::generate(&env);
        let amm_pool = env.register_contract(None, StubAmmPool);
        let cl_pool = env.register_contract(None, MockClPool);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);
        consumer.propose_keeper(&nominee);
        consumer.accept_keeper(&nominee);

        consumer.save_snapshot(&amm_pool);
        env.ledger().with_mut(|l| l.timestamp += 10);
        consumer.save_snapshot(&amm_pool);
        env.ledger().with_mut(|l| l.timestamp += 10);
        consumer.save_snapshot(&amm_pool);
        consumer.save_cl_snapshot(&cl_pool);
        consumer.add_tracked_pool(&Address::generate(&env), &PoolType::Amm);
        consumer.remove_tracked_pool(&amm_pool);
        consumer.delete_snapshot(&amm_pool, &10_010);
        assert_eq!(consumer.get_snapshot_count(&amm_pool), 2);
    }

    /// Upload this contract's own release WASM, built by `build_workspace.sh`
    /// before `cargo test` (same approach as `router` / `batch_router`).
    fn upload_own_wasm(env: &Env) -> soroban_sdk::BytesN<32> {
        env.budget().reset_unlimited();
        let wasm = soroban_sdk::Bytes::from_slice(
            env,
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../target/wasm32v1-none/release/twal_consumer.wasm"
            )),
        );
        env.deployer().upload_contract_wasm(wasm)
    }

    #[test]
    fn upgrade_rejects_non_keeper() {
        let env = Env::default();
        let keeper = Address::generate(&env);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);
        let wasm_hash = upload_own_wasm(&env);

        assert!(consumer.try_upgrade(&wasm_hash).is_err());
    }

    #[test]
    fn keeper_upgrade_preserves_snapshots() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(10_000);
        let keeper = Address::generate(&env);
        let pool = env.register_contract(None, StubAmmPool);
        let consumer_addr = env.register_contract(None, TwalConsumer);
        let consumer = TwalConsumerClient::new(&env, &consumer_addr);
        consumer.initialize(&keeper);
        consumer.save_snapshot(&pool);
        let wasm_hash = upload_own_wasm(&env);

        consumer.upgrade(&wasm_hash);

        assert_eq!(consumer.get_snapshot_count(&pool), 1);
        assert_eq!(consumer.max_servable_window(&pool), 0);
    }

    #[test]
    fn deleting_newest_snapshot_is_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (pool, consumer) = setup_amm_pool(&env);
        consumer.save_snapshot(&pool);
        env.ledger().with_mut(|l| l.timestamp += 10);
        consumer.save_snapshot(&pool);
        env.ledger().with_mut(|l| l.timestamp += 10);
        consumer.save_snapshot(&pool);

        assert_eq!(
            consumer.try_delete_snapshot(&pool, &10_020),
            Err(Ok(TwalError::CannotDeleteRequiredSnapshot))
        );
    }

    #[test]
    fn deleting_when_only_two_snapshots_remain_is_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (pool, consumer) = setup_amm_pool(&env);
        consumer.save_snapshot(&pool);
        env.ledger().with_mut(|l| l.timestamp += 10);
        consumer.save_snapshot(&pool);

        assert_eq!(
            consumer.try_delete_snapshot(&pool, &10_000),
            Err(Ok(TwalError::CannotDeleteRequiredSnapshot))
        );
    }

    #[test]
    fn max_servable_window_tracks_oldest_of_three_snapshots() {
        let env = Env::default();
        env.mock_all_auths();
        let (pool, consumer) = setup_amm_pool(&env);
        consumer.save_snapshot(&pool);
        env.ledger().with_mut(|l| l.timestamp = 10_120);
        consumer.save_snapshot(&pool);
        env.ledger().with_mut(|l| l.timestamp = 10_250);
        consumer.save_snapshot(&pool);
        env.ledger().with_mut(|l| l.timestamp = 10_500);

        assert_eq!(consumer.max_servable_window(&pool), 500);
    }

    #[test]
    fn max_servable_window_is_zero_for_untracked_pool() {
        let env = Env::default();
        let (_keeper, consumer) = setup_consumer(&env);
        assert_eq!(consumer.max_servable_window(&Address::generate(&env)), 0);
    }
}
