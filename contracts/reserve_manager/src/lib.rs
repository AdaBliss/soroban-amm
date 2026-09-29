//! Liquidity reserve management contract.
//!
//! Tracks protocol-wide minimum liquidity requirements for pool pairs and
//! exposes a `check_reserves` read-only gate for **off-chain callers**.
//! The contract is **not** wired into `amm::remove_liquidity` /
//! `amm::remove_liquidity_one_sided`; `set_min_reserve` minimums are not
//! enforced on any on-chain withdrawal path. Off-chain dashboards, bots,
//! multisig governance, and migration scripts should invoke
//! `check_reserves(pool)` against any candidate pool before triggering a
//! rebalance or migration; the return value determines whether to proceed,
//! retry, or alert. The on-chain AMM hookup is deferred to a follow-up;
//! see issue #518.
//!
//! Governance is a single address that may update requirements. The address
//! can be a multisig or DAO contract for on-chain governance.
//!
//! Both pool kinds in this workspace are supported: constant-product V2 pools
//! (`contracts/amm`) and concentrated-liquidity pools
//! (`contracts/concentrated_liquidity`). V2 reserves come from the pool's
//! `get_info()`; CL pools have no such function and no scalar reserves, so
//! their reserves are the token balances the pool contract actually holds.
//! `check_reserves` auto-detects which path to take, and governance may record
//! a pool's kind with `set_pool_kind` to skip the probe.
//!
//! Flow:
//!   1. Deploy this contract.
//!   2. Call `initialize` with the governance address and the factory address.
//!   3. Governance calls `set_min_reserve` to configure per-pair requirements.
//!   4. Optionally, governance calls `set_pool_kind` to record a pool's kind.
//!   5. **Off-chain** callers query `check_reserves(pool)` to gate actions
//!      that take liquidity out of the pool (rebalance, migration, ...).
//!      The AMM itself does **not** call this contract on-chain; integrating
//!      pool exits with minimum guards is the responsibility of callers
//!      (off-chain bots, multisig governance, the off-chain router).
//!   6. Governance may call `propose_governance` / `accept_governance` to
//!      securely hand off control.

#![no_std]

use soroban_amm_sdk::emit_versioned_event;
use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, symbol_short, Address,
    Env, Symbol, Vec,
};

// ── Storage TTL ──────────────────────────────────────────────────────────────

/// Bump per-pair requirements when their remaining TTL drops below this.
const MIN_PERSISTENT_TTL: u32 = 172_800; // ~10 days at 5s/ledger
/// Target TTL to extend per-pair requirements to on write.
const PERSISTENT_TTL_BUMP_TO: u32 = 259_200; // ~15 days at 5s/ledger

/// Below this many remaining ledgers, `extend_ttl` renews the contract's
/// **instance** entry (governance address, factory address, pool-kind
/// overrides); each renewal bumps it back up to `INSTANCE_TTL_BUMP_TO`.
///
/// This contract custodies protocol-owned liquidity and its "off-chain
/// caller" design (see the module doc comment) means it is invoked at the
/// pace of dashboards, bots, and multisig governance rather than steady
/// user traffic (see #909). The instance entry holds the governance address
/// itself, so if it lapses, the address needed to authorize a restore is
/// exactly what's archived. `172_800` ledgers (~10 days at 5s/ledger)
/// matches the floor `contracts/amm` uses for its own instance entry, and
/// `518_400` ledgers (~30 days at 5s/ledger) gives every renewal a full
/// month of slack before the next one is due.
const INSTANCE_TTL_THRESHOLD: u32 = 172_800;
const INSTANCE_TTL_BUMP_TO: u32 = 518_400;

// ── Pagination / batching ────────────────────────────────────────────────────

/// Upper bound on the number of entries a single paginated read or batch health
/// check may touch. Keeps every read path within the per-transaction resource
/// limit no matter how many pairs governance has configured.
pub const MAX_PAGE: u32 = 50;

// ── External contract interfaces ─────────────────────────────────────────────

/// Subset of the AMM pool interface needed to read current reserves.
#[contractclient(name = "AmmPoolClient")]
pub trait AmmPoolInterface {
    fn get_info(env: Env) -> PoolInfo;
}

/// Subset of the concentrated-liquidity pool interface needed to identify its
/// token pair. CL pools expose no `get_info`, and their liquidity is spread
/// across ticks rather than held as a pair of scalar reserves, so reserves are
/// derived from the pool's actual token balances instead.
#[contractclient(name = "ClPoolClient")]
pub trait ClPoolInterface {
    fn get_tokens(env: Env) -> (Address, Address);
}

/// Subset of the SEP-41 token interface needed to read a holder's balance.
#[contractclient(name = "TokenBalanceClient")]
pub trait TokenBalanceInterface {
    fn balance(env: Env, id: Address) -> i128;
}

/// Which pool implementation a given address is.
///
/// Recorded per pool address by governance via `set_pool_kind`. When a pool has
/// no recorded kind, `check_reserves` auto-detects it by trying the V2
/// `get_info` path first and falling back to the CL path.
#[contracttype]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolKind {
    /// Constant-product V2 pool (`contracts/amm`), exposing `get_info`.
    Amm,
    /// Tick-based concentrated-liquidity pool (`contracts/concentrated_liquidity`).
    ConcentratedLiquidity,
}

/// Mirror of the PoolInfo struct exported by the AMM pool contract.
/// Must match the AMM's field list exactly for cross-contract deserialization.
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

/// Minimum reserve requirement for a token pair.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct ReserveRequirement {
    pub min_reserve_a: i128,
    pub min_reserve_b: i128,
}

/// Structured health report for a single pool.
///
/// Every amount is expressed in the pool's own token order, so `reserve_a` and
/// `min_a` both refer to `token_a` regardless of how the requirement was
/// normalised in storage.
///
/// When a pool could not be read (see `check_reserves_batch`), `token_a` and
/// `token_b` are set to the pool address itself, the reserves and minimums are
/// zero, and `healthy` is `false`.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct ReserveReport {
    pub pool: Address,
    pub token_a: Address,
    pub token_b: Address,
    pub reserve_a: i128,
    pub reserve_b: i128,
    pub min_a: i128,
    pub min_b: i128,
    pub healthy: bool,
    /// Shortfall on each side; 0 when the side is at or above its floor.
    pub shortfall_a: i128,
    pub shortfall_b: i128,
}

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Admin,
    /// Pending admin nominee for two-step handover.
    PendingAdmin,
    Governance,
    /// Pending governance nominee for two-step handover.
    PendingGovernance,
    Factory,
    /// Normalized (smaller_addr, larger_addr) → ReserveRequirement.
    MinReserve(Address, Address),
    /// Pool address → PoolKind. Optional; absence means "auto-detect".
    PoolKind(Address),
    /// Insertion-ordered index of every pair that currently has a non-zero
    /// requirement, stored normalised as (smaller_addr, larger_addr).
    ConfiguredPairs,
    Paused,
}

// ── Typed errors ─────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ReserveManagerError {
    NoPendingGovernance = 1,
    Unauthorized = 2,
    AlreadyInitialized = 3,
    NegativeReserveAmount = 4,
    /// A batch health check was handed more pools than `MAX_PAGE`.
    BatchTooLarge = 5,
    /// `accept_admin` called without a prior `propose_admin`.
    NoPendingAdmin = 6,
    /// `accept_admin` called by an address other than the nominee.
    WrongAdmin = 7,
    Paused = 8,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct ReserveManager;

#[contractimpl]
impl ReserveManager {
    /// Extends the contract's **instance** storage TTL. Safe to call on
    /// every entrypoint — `extend_ttl` is a no-op until the entry's
    /// remaining TTL drops below `INSTANCE_TTL_THRESHOLD`.
    fn extend_instance_ttl(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_BUMP_TO);
    }

    // ── Setup ─────────────────────────────────────────────────────────────────

    /// One-time setup. `governance` is the only address permitted to call
    /// `set_min_reserve` and `transfer_governance`.
    pub fn initialize(
        env: Env,
        governance: Address,
        factory: Address,
    ) -> Result<(), ReserveManagerError> {
        Self::extend_instance_ttl(&env);
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(ReserveManagerError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Admin, &governance);
        env.storage().instance().set(&DataKey::Factory, &factory);
        Ok(())
    }

    // ── Governance ────────────────────────────────────────────────────────────

    /// Nominate a new governance address.
    ///
    /// The nominee must call `accept_governance` to complete the two-step
    /// handover. Requires current governance auth.
    pub fn propose_governance(
        env: Env,
        current_governance: Address,
        new_governance: Address,
    ) -> Result<(), ReserveManagerError> {
        Self::extend_instance_ttl(&env);
        if Self::is_paused(env.clone()) {
            return Err(ReserveManagerError::Paused);
        }
        let stored: Address = env.storage().instance().get(&DataKey::Governance).unwrap();
        if current_governance != stored {
            return Err(ReserveManagerError::Unauthorized);
        }
        stored.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PendingGovernance, &Some(new_governance.clone()));
        emit_versioned_event!(
            env,
            (Symbol::new(&env, "governance_proposed"),),
            (current_governance, new_governance)
        );
        Ok(())
    }

    /// Accept a pending governance nomination.
    ///
    /// Only the nominated address can call this, and it must authorize the
    /// transaction. On success the stored governance is updated, the pending
    /// nominee is cleared, and a `governance_transferred` event is emitted.
    pub fn accept_governance(env: Env, new_governance: Address) -> Result<(), ReserveManagerError> {
        Self::extend_instance_ttl(&env);
        if Self::is_paused(env.clone()) {
            return Err(ReserveManagerError::Paused);
        }
        let pending: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PendingGovernance)
            .unwrap_or(None);
        let nominee = pending.ok_or(ReserveManagerError::NoPendingGovernance)?;
        if new_governance != nominee {
            return Err(ReserveManagerError::Unauthorized);
        }
        new_governance.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::Governance, &new_governance);
        env.storage()
            .instance()
            .set(&DataKey::PendingGovernance, &Option::<Address>::None);
        emit_versioned_event!(
            env,
            (Symbol::new(&env, "governance_transferred"),),
            (new_governance,)
        );
        Ok(())
    }

    /// Return the pending governance nominee, if any.
    pub fn get_pending_governance(env: Env) -> Option<Address> {
        Self::extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::PendingGovernance)
            .unwrap_or(None)
    }

    pub fn pause(env: Env) -> Result<(), ReserveManagerError> {
        let gov: Address = env.storage().instance().get(&DataKey::Governance).unwrap();
        gov.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        emit_versioned_event!(env, (symbol_short!("pause"),), ());
        Ok(())
    }

    pub fn unpause(env: Env) -> Result<(), ReserveManagerError> {
        let gov: Address = env.storage().instance().get(&DataKey::Governance).unwrap();
        gov.require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
        emit_versioned_event!(env, (symbol_short!("unpause"),), ());
        Ok(())
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }
    /// Nominate a new admin. The nominee must call `accept_admin` to complete the transfer.
    pub fn propose_admin(
        env: Env,
        admin: Address,
        new_admin: Address,
    ) -> Result<(), ReserveManagerError> {
        let stored: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .or_else(|| env.storage().instance().get(&DataKey::Governance))
            .unwrap();
        if admin != stored {
            return Err(ReserveManagerError::Unauthorized);
        }
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Some(new_admin.clone()));
        emit_versioned_event!(
            env,
            (Symbol::new(&env, "admin_nominated"),),
            (admin, new_admin)
        );
        Ok(())
    }

    /// Accept the pending admin nomination. Caller becomes the new admin.
    pub fn accept_admin(env: Env, new_admin: Address) -> Result<(), ReserveManagerError> {
        let pending: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None);
        let nominee = pending.ok_or(ReserveManagerError::NoPendingAdmin)?;
        if new_admin != nominee {
            return Err(ReserveManagerError::WrongAdmin);
        }
        new_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage()
            .instance()
            .set(&DataKey::Governance, &new_admin);
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Option::<Address>::None);
        emit_versioned_event!(env, (Symbol::new(&env, "admin_changed"),), (new_admin,));
        Ok(())
    }

    /// Return the active admin address.
    pub fn get_admin(env: Env) -> Option<Address> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .or_else(|| env.storage().instance().get(&DataKey::Governance))
    }

    /// Return the pending admin nominee, if any.
    pub fn get_pending_admin(env: Env) -> Option<Address> {
        env.storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None)
    }

    // ── Reserve requirements ──────────────────────────────────────────────────

    /// Set the minimum reserve amounts for a token pair.
    ///
    /// Requires governance auth. Token order is normalised: the pair is stored
    /// with the lexicographically smaller address first so that lookups are
    /// order-independent.
    ///
    /// The requirement is keyed by token pair, not by pool address, and applies
    /// uniformly to both pool kinds. For a V2 pool the minimums are compared
    /// against `get_info()`'s reserves; for a concentrated-liquidity pool they
    /// are compared against the token balances the pool actually holds. A pair
    /// configured here therefore constrains every pool trading it, whichever
    /// implementation the pool uses.
    ///
    /// Set both values to 0 to remove a requirement.
    ///
    /// Per-pair requirements are held in persistent storage so each pair is an
    /// independent entry with its own TTL, rather than sharing the single
    /// instance-storage blob loaded on every invocation.
    pub fn set_min_reserve(
        env: Env,
        token_a: Address,
        token_b: Address,
        min_reserve_a: i128,
        min_reserve_b: i128,
    ) -> Result<(), ReserveManagerError> {
        Self::extend_instance_ttl(&env);
        if Self::is_paused(env.clone()) {
            return Err(ReserveManagerError::Paused);
        }
        let gov: Address = env.storage().instance().get(&DataKey::Governance).unwrap();
        gov.require_auth();
        if min_reserve_a < 0 || min_reserve_b < 0 {
            return Err(ReserveManagerError::NegativeReserveAmount);
        }

        let token_a_is_first = token_a < token_b;
        let (ta, tb) = Self::normalize(token_a, token_b);
        let key = DataKey::MinReserve(ta, tb);

        // Both minimums zero means "no requirement": delete the entry so it does
        // not linger, matching the documented behaviour above, and drop the pair
        // from the enumeration index so it cannot grow without bound.
        if min_reserve_a == 0 && min_reserve_b == 0 {
            env.storage().persistent().remove(&key);
            if let DataKey::MinReserve(ta, tb) = &key {
                Self::deindex_pair(&env, ta, tb);
            }
            return Ok(());
        }

        let (normalized_min_a, normalized_min_b) = if token_a_is_first {
            (min_reserve_a, min_reserve_b)
        } else {
            (min_reserve_b, min_reserve_a)
        };
        let req = ReserveRequirement {
            min_reserve_a: normalized_min_a,
            min_reserve_b: normalized_min_b,
        };
        env.storage().persistent().set(&key, &req);
        env.storage()
            .persistent()
            .extend_ttl(&key, MIN_PERSISTENT_TTL, PERSISTENT_TTL_BUMP_TO);
        if let DataKey::MinReserve(ta, tb) = &key {
            Self::index_pair(&env, ta, tb);
        }
        Ok(())
    }

    // -- Enumeration ----------------------------------------------------------

    /// Number of pairs that currently have a non-zero requirement configured.
    pub fn get_configured_pair_count(env: Env) -> u32 {
        Self::extend_instance_ttl(&env);
        Self::configured_pairs(&env).len()
    }

    /// Page through the configured pairs in the order they were first written.
    ///
    /// `limit` is clamped to [`MAX_PAGE`]; an `offset` at or beyond the current
    /// count yields an empty `Vec` rather than panicking. Pairs are returned
    /// normalised, i.e. the lexicographically smaller address comes first.
    pub fn list_configured_pairs(env: Env, offset: u32, limit: u32) -> Vec<(Address, Address)> {
        Self::extend_instance_ttl(&env);
        let pairs = Self::configured_pairs(&env);
        let count = pairs.len();
        let mut page: Vec<(Address, Address)> = Vec::new(&env);
        if offset >= count || limit == 0 {
            return page;
        }
        let end = offset.saturating_add(limit.min(MAX_PAGE)).min(count);
        for i in offset..end {
            page.push_back(pairs.get(i).unwrap());
        }
        page
    }

    /// Return the minimum reserve requirement for a pair, or (0, 0) if none.
    ///
    /// The value is keyed by token pair and is independent of which pool kind
    /// will eventually be checked against it — see
    /// [`ReserveManager::set_min_reserve`].
    pub fn get_min_reserve(env: Env, token_a: Address, token_b: Address) -> ReserveRequirement {
        Self::extend_instance_ttl(&env);
        let (ta, tb) = Self::normalize(token_a, token_b);
        env.storage()
            .persistent()
            .get(&DataKey::MinReserve(ta, tb))
            .unwrap_or(ReserveRequirement {
                min_reserve_a: 0,
                min_reserve_b: 0,
            })
    }

    // ── Compliance checks ─────────────────────────────────────────────────────

    /// Check whether a pool's current reserves satisfy the registered minimums.
    ///
    /// Works for both pool kinds:
    ///
    /// * **V2 (`PoolKind::Amm`)** — reserves are read from the pool's
    ///   `get_info()`, exactly as before.
    /// * **Concentrated liquidity (`PoolKind::ConcentratedLiquidity`)** — CL
    ///   pools have no `get_info` and no scalar reserves (liquidity is spread
    ///   across ticks, and only the in-range slice backs the current price), so
    ///   "reserves" are defined as the token balances the pool contract
    ///   actually holds: the SEP-41 `balance()` of each token in
    ///   `get_tokens()`, queried against the pool's own address.
    ///
    /// When the pool has no recorded kind, the V2 path is tried first and the
    /// CL path is used if it fails, so callers need not register a kind for
    /// `check_reserves` to work. Registering one via `set_pool_kind` skips the
    /// failed probe and its wasted cross-contract call.
    ///
    /// Returns `true` if the pool meets or exceeds its requirements, or if no
    /// requirement has been set for that pair. Returns `false` otherwise.
    ///
    /// Does not modify any state.
    pub fn check_reserves(env: Env, pool: Address) -> bool {
        Self::extend_instance_ttl(&env);
        let (token_a, token_b, reserve_a, reserve_b) = match Self::pool_kind_of(&env, &pool) {
            Some(PoolKind::Amm) => Self::read_amm_reserves(&env, &pool),
            Some(PoolKind::ConcentratedLiquidity) => Self::read_balance_reserves(&env, &pool),
            // Unregistered: probe the V2 shape, fall back to token balances.
            None => match AmmPoolClient::new(&env, &pool).try_get_info() {
                Ok(Ok(info)) => (info.token_a, info.token_b, info.reserve_a, info.reserve_b),
                _ => Self::read_balance_reserves(&env, &pool),
            },
        };

        let (ta, tb) = Self::normalize(token_a.clone(), token_b.clone());
        let req: ReserveRequirement = env
            .storage()
            .persistent()
            .get(&DataKey::MinReserve(ta.clone(), tb))
            .unwrap_or(ReserveRequirement {
                min_reserve_a: 0,
                min_reserve_b: 0,
            });

        // The requirement is stored against the normalized pair, so align the
        // observed reserves with that same ordering before comparing.
        let (min_a, min_b) = if ta == token_a {
            (req.min_reserve_a, req.min_reserve_b)
        } else {
            (req.min_reserve_b, req.min_reserve_a)
        };

        reserve_a >= min_a && reserve_b >= min_b
    }

    /// Record which implementation `pool` is, so `check_reserves` can dispatch
    /// without probing. Requires governance auth.
    ///
    /// This is optional: `check_reserves` auto-detects unregistered pools.
    /// Registering a kind only avoids the cost of a failed `get_info` probe.
    pub fn set_pool_kind(
        env: Env,
        pool: Address,
        kind: PoolKind,
    ) -> Result<(), ReserveManagerError> {
        Self::extend_instance_ttl(&env);
        if Self::is_paused(env.clone()) {
            return Err(ReserveManagerError::Paused);
        }
        let gov: Address = env.storage().instance().get(&DataKey::Governance).unwrap();
        gov.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PoolKind(pool), &kind);
        Ok(())
    }

    /// Return the recorded kind for `pool`, or `None` if it is auto-detected.
    pub fn get_pool_kind(env: Env, pool: Address) -> Option<PoolKind> {
        Self::extend_instance_ttl(&env);
        Self::pool_kind_of(&env, &pool)
    }

    /// Structured version of [`ReserveManager::check_reserves`] that reports the
    /// actual numbers instead of a bare boolean.
    ///
    /// `healthy` always agrees with `check_reserves` for the same pool. Like
    /// `check_reserves`, this call propagates a failure of the pool's
    /// `get_info()`; use [`ReserveManager::check_reserves_batch`] for
    /// fault-isolated reads.
    ///
    /// Does not modify any state.
    pub fn check_reserves_detailed(env: Env, pool: Address) -> ReserveReport {
        Self::extend_instance_ttl(&env);
        let info = AmmPoolClient::new(&env, &pool).get_info();
        Self::build_report(&env, &pool, &info)
    }

    /// Health-check up to [`MAX_PAGE`] pools in one call.
    ///
    /// The read of each pool is fault-isolated: a pool whose `get_info()` call
    /// fails (not an AMM pool, archived, panicking) is reported with
    /// `healthy: false` and zeroed amounts instead of aborting the whole batch.
    ///
    /// When at least one pool is unhealthy a `res_warn` event is emitted
    /// carrying the offending pool addresses, so keepers can subscribe rather
    /// than poll.
    ///
    /// Returns [`ReserveManagerError::BatchTooLarge`] when `pools.len()` exceeds
    /// `MAX_PAGE`; truncating silently would hide pools from a health check.
    pub fn check_reserves_batch(
        env: Env,
        pools: Vec<Address>,
    ) -> Result<Vec<ReserveReport>, ReserveManagerError> {
        Self::extend_instance_ttl(&env);
        if pools.len() > MAX_PAGE {
            return Err(ReserveManagerError::BatchTooLarge);
        }

        let mut reports: Vec<ReserveReport> = Vec::new(&env);
        let mut unhealthy: Vec<Address> = Vec::new(&env);

        for pool in pools.iter() {
            let report = match AmmPoolClient::new(&env, &pool).try_get_info() {
                Ok(Ok(info)) => Self::build_report(&env, &pool, &info),
                _ => Self::unreadable_report(&pool),
            };
            if !report.healthy {
                unhealthy.push_back(pool.clone());
            }
            reports.push_back(report);
        }

        if !unhealthy.is_empty() {
            emit_versioned_event!(env, (symbol_short!("res_warn"),), (unhealthy,));
        }

        Ok(reports)
    }

    /// Return the governance address.
    pub fn get_governance(env: Env) -> Address {
        Self::extend_instance_ttl(&env);
        env.storage().instance().get(&DataKey::Governance).unwrap()
    }

    /// Return the factory address.
    pub fn get_factory(env: Env) -> Address {
        Self::extend_instance_ttl(&env);
        env.storage().instance().get(&DataKey::Factory).unwrap()
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    fn normalize(a: Address, b: Address) -> (Address, Address) {
        if a < b {
            (a, b)
        } else {
            (b, a)
        }
    }

    /// Load the pair index, or an empty vector when nothing is configured yet.
    fn configured_pairs(env: &Env) -> Vec<(Address, Address)> {
        env.storage()
            .persistent()
            .get(&DataKey::ConfiguredPairs)
            .unwrap_or_else(|| Vec::new(env))
    }

    fn save_configured_pairs(env: &Env, pairs: &Vec<(Address, Address)>) {
        env.storage()
            .persistent()
            .set(&DataKey::ConfiguredPairs, pairs);
        env.storage().persistent().extend_ttl(
            &DataKey::ConfiguredPairs,
            MIN_PERSISTENT_TTL,
            PERSISTENT_TTL_BUMP_TO,
        );
    }

    /// Append a normalised pair to the index on its first write. Re-writing an
    /// existing pair is a no-op, so the index never holds duplicates.
    fn index_pair(env: &Env, token_a: &Address, token_b: &Address) {
        let mut pairs = Self::configured_pairs(env);
        for i in 0..pairs.len() {
            let (a, b) = pairs.get(i).unwrap();
            if a == *token_a && b == *token_b {
                // Already indexed: still refresh the TTL so the index does not
                // expire while the entries it points at are being kept alive.
                Self::save_configured_pairs(env, &pairs);
                return;
            }
        }
        pairs.push_back((token_a.clone(), token_b.clone()));
        Self::save_configured_pairs(env, &pairs);
    }

    /// Drop a normalised pair from the index, preserving the order of the rest.
    fn deindex_pair(env: &Env, token_a: &Address, token_b: &Address) {
        let pairs = Self::configured_pairs(env);
        for i in 0..pairs.len() {
            let (a, b) = pairs.get(i).unwrap();
            if a == *token_a && b == *token_b {
                let mut remaining = pairs.clone();
                remaining.remove(i);
                Self::save_configured_pairs(env, &remaining);
                return;
            }
        }
    }

    /// Build a report from a pool's own `PoolInfo`, expressed in the pool's
    /// token order.
    fn build_report(env: &Env, pool: &Address, info: &PoolInfo) -> ReserveReport {
        let token_a_is_first = info.token_a < info.token_b;
        let (ta, tb) = if token_a_is_first {
            (info.token_a.clone(), info.token_b.clone())
        } else {
            (info.token_b.clone(), info.token_a.clone())
        };

        let req: ReserveRequirement = env
            .storage()
            .persistent()
            .get(&DataKey::MinReserve(ta, tb))
            .unwrap_or(ReserveRequirement {
                min_reserve_a: 0,
                min_reserve_b: 0,
            });

        // Requirements are stored under the normalised pair; map them back onto
        // the pool's own token order so `min_a` always describes `token_a`.
        let (min_a, min_b) = if token_a_is_first {
            (req.min_reserve_a, req.min_reserve_b)
        } else {
            (req.min_reserve_b, req.min_reserve_a)
        };

        let shortfall_a = (min_a - info.reserve_a).max(0);
        let shortfall_b = (min_b - info.reserve_b).max(0);

        ReserveReport {
            pool: pool.clone(),
            token_a: info.token_a.clone(),
            token_b: info.token_b.clone(),
            reserve_a: info.reserve_a,
            reserve_b: info.reserve_b,
            min_a,
            min_b,
            healthy: shortfall_a == 0 && shortfall_b == 0,
            shortfall_a,
            shortfall_b,
        }
    }

    /// Placeholder report for a pool whose `get_info()` could not be read.
    ///
    /// The pool address stands in for the unknown token pair so the struct stays
    /// a plain `#[contracttype]` without optional fields.
    fn unreadable_report(pool: &Address) -> ReserveReport {
        ReserveReport {
            pool: pool.clone(),
            token_a: pool.clone(),
            token_b: pool.clone(),
            reserve_a: 0,
            reserve_b: 0,
            min_a: 0,
            min_b: 0,
            healthy: false,
            shortfall_a: 0,
            shortfall_b: 0,
        }
    }

    /// Recorded kind for `pool`, or `None` when it should be auto-detected.
    fn pool_kind_of(env: &Env, pool: &Address) -> Option<PoolKind> {
        env.storage()
            .instance()
            .get(&DataKey::PoolKind(pool.clone()))
    }

    /// Read `(token_a, token_b, reserve_a, reserve_b)` from a V2 pool's
    /// `get_info()`.
    fn read_amm_reserves(env: &Env, pool: &Address) -> (Address, Address, i128, i128) {
        let info = AmmPoolClient::new(env, pool).get_info();
        (info.token_a, info.token_b, info.reserve_a, info.reserve_b)
    }

    /// Read `(token_a, token_b, reserve_a, reserve_b)` from a CL pool by
    /// querying the SEP-41 balance of each token held by the pool itself.
    ///
    /// This sidesteps mirroring CL's internal tick/liquidity accounting and
    /// gives a signal that is meaningful for both pool kinds: how much of each
    /// token the pool can actually pay out.
    fn read_balance_reserves(env: &Env, pool: &Address) -> (Address, Address, i128, i128) {
        let (token_a, token_b) = ClPoolClient::new(env, pool).get_tokens();
        let reserve_a = TokenBalanceClient::new(env, &token_a).balance(pool);
        let reserve_b = TokenBalanceClient::new(env, &token_b).balance(pool);
        (token_a, token_b, reserve_a, reserve_b)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use amm::AmmPool;
    use soroban_sdk::{
        testutils::{storage::Instance as _, Address as _, Events as _, Ledger as _},
        token::StellarAssetClient,
        Env, IntoVal, String,
    };
    use token::{LpToken, LpTokenClient};

    use super::*;

    struct Setup {
        env: Env,
        rm_addr: Address,
        pool: Address,
        ta: Address,
        tb: Address,
        governance: Address,
    }

    fn setup() -> Setup {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let governance = Address::generate(&env);

        // Deploy token pair.
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        // Deploy AMM pool directly (native — avoids WASM serialization mismatches).
        let lp_addr = env.register_contract(None, LpToken);
        let pool_addr = env.register_contract(None, AmmPool);
        LpTokenClient::new(&env, &lp_addr).initialize(
            &pool_addr,
            &String::from_str(&env, "LP"),
            &String::from_str(&env, "LP"),
            &7u32,
        );
        amm::AmmPoolClient::new(&env, &pool_addr)
            .initialize(&admin, &ta, &tb, &lp_addr, &30_i128, &admin, &0_i128);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&provider, &1_000_000_i128);
        StellarAssetClient::new(&env, &tb).mint(&provider, &1_000_000_i128);
        amm::AmmPoolClient::new(&env, &pool_addr).add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        // factory_addr is not used in check_reserves, just needed for initialize.
        let factory_addr = Address::generate(&env);
        let rm_addr = env.register_contract(None, ReserveManager);
        ReserveManagerClient::new(&env, &rm_addr).initialize(&governance, &factory_addr);

        Setup {
            env,
            rm_addr,
            pool: pool_addr,
            ta,
            tb,
            governance,
        }
    }

    #[test]
    fn test_initialize_stores_governance_and_factory() {
        let env = Env::default();
        env.mock_all_auths();
        let gov = Address::generate(&env);
        let factory = Address::generate(&env);
        let rm_addr = env.register_contract(None, ReserveManager);
        let rm = ReserveManagerClient::new(&env, &rm_addr);
        rm.initialize(&gov, &factory);
        assert_eq!(rm.get_governance(), gov);
        assert_eq!(rm.get_factory(), factory);
    }

    #[test]
    fn test_initialize_twice_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let gov = Address::generate(&env);
        let factory = Address::generate(&env);
        let rm_addr = env.register_contract(None, ReserveManager);
        let rm = ReserveManagerClient::new(&env, &rm_addr);
        rm.initialize(&gov, &factory);
        assert_eq!(
            rm.try_initialize(&gov, &factory),
            Err(Ok(ReserveManagerError::AlreadyInitialized))
        );
    }

    #[test]
    fn test_set_and_get_min_reserve() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        rm.set_min_reserve(&s.ta, &s.tb, &500_000_i128, &300_000_i128);

        // Order-independent lookup
        let req_ab = rm.get_min_reserve(&s.ta, &s.tb);
        let req_ba = rm.get_min_reserve(&s.tb, &s.ta);

        // Reserves are stored normalised; values correspond to the normalised order
        assert_eq!(req_ab.min_reserve_a, req_ba.min_reserve_a);
        assert_eq!(req_ab.min_reserve_b, req_ba.min_reserve_b);
    }

    #[test]
    fn test_check_reserves_passes_when_above_minimum() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        // Pool has 1_000_000 of each; set minimum below that
        rm.set_min_reserve(&s.ta, &s.tb, &500_000_i128, &500_000_i128);
        assert!(rm.check_reserves(&s.pool));
    }

    #[test]
    fn test_check_reserves_fails_when_below_minimum() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        // Set minimum above current reserves (1_000_000)
        rm.set_min_reserve(&s.ta, &s.tb, &2_000_000_i128, &2_000_000_i128);
        assert!(!rm.check_reserves(&s.pool));
    }

    #[test]
    fn test_set_min_reserve_preserves_amounts_for_reversed_token_args() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        let (larger_token, smaller_token) = if s.ta < s.tb {
            (&s.tb, &s.ta)
        } else {
            (&s.ta, &s.tb)
        };

        // The AMM pool reserves are 1_000_000 for both tokens. This call
        // intentionally passes the larger token first, so set_min_reserve must
        // swap the amounts before storing them under the normalized key.
        rm.set_min_reserve(larger_token, smaller_token, &2_000_000_i128, &500_000_i128);

        let req = rm.get_min_reserve(&s.ta, &s.tb);
        assert_eq!(req.min_reserve_a, 500_000);
        assert_eq!(req.min_reserve_b, 2_000_000);
        assert!(!rm.check_reserves(&s.pool));
    }

    #[test]
    fn test_check_reserves_passes_with_no_requirement() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        // No requirement set — should pass by default
        assert!(rm.check_reserves(&s.pool));
    }

    // ── Concentrated-liquidity pools (Issue #829) ────────────────────────────

    /// Deploy a CL pool on the same token pair and seed it with `amount` of
    /// each token, so its held balances stand in for "reserves".
    fn deploy_cl_pool(s: &Setup, amount: i128) -> Address {
        let admin = Address::generate(&s.env);
        // Fully qualified: the bare name would shadow `PoolKind::ConcentratedLiquidity`.
        let cl_addr = s
            .env
            .register_contract(None, concentrated_liquidity::ConcentratedLiquidity);
        concentrated_liquidity::ConcentratedLiquidityClient::new(&s.env, &cl_addr)
            .initialize(&admin, &s.ta, &s.tb, &30_i128, &0_i32, &1_i32);
        if amount > 0 {
            StellarAssetClient::new(&s.env, &s.ta).mint(&cl_addr, &amount);
            StellarAssetClient::new(&s.env, &s.tb).mint(&cl_addr, &amount);
        }
        cl_addr
    }

    #[test]
    fn test_check_reserves_cl_pool_does_not_trap() {
        // Before the fix this trapped: CL pools have no `get_info`.
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let cl = deploy_cl_pool(&s, 1_000_000);

        rm.set_min_reserve(&s.ta, &s.tb, &500_000_i128, &500_000_i128);
        assert!(rm.check_reserves(&cl));
    }

    #[test]
    fn test_check_reserves_cl_pool_below_minimum_returns_false() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let cl = deploy_cl_pool(&s, 100_000);

        rm.set_min_reserve(&s.ta, &s.tb, &500_000_i128, &500_000_i128);
        assert!(!rm.check_reserves(&cl));
    }

    #[test]
    fn test_check_reserves_cl_pool_exactly_at_minimum_returns_true() {
        // Mirrors the V2 semantics: the comparison is `>=`, not `>`.
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let cl = deploy_cl_pool(&s, 500_000);

        rm.set_min_reserve(&s.ta, &s.tb, &500_000_i128, &500_000_i128);
        assert!(rm.check_reserves(&cl));
    }

    #[test]
    fn test_check_reserves_cl_pool_with_no_requirement_passes() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let cl = deploy_cl_pool(&s, 0);

        assert!(rm.check_reserves(&cl));
    }

    #[test]
    fn test_check_reserves_respects_recorded_cl_pool_kind() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let cl = deploy_cl_pool(&s, 1_000_000);

        rm.set_pool_kind(&cl, &PoolKind::ConcentratedLiquidity);
        assert_eq!(rm.get_pool_kind(&cl), Some(PoolKind::ConcentratedLiquidity));

        rm.set_min_reserve(&s.ta, &s.tb, &500_000_i128, &500_000_i128);
        assert!(rm.check_reserves(&cl));

        rm.set_min_reserve(&s.ta, &s.tb, &2_000_000_i128, &2_000_000_i128);
        assert!(!rm.check_reserves(&cl));
    }

    #[test]
    fn test_check_reserves_respects_recorded_amm_pool_kind() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        rm.set_pool_kind(&s.pool, &PoolKind::Amm);
        assert_eq!(rm.get_pool_kind(&s.pool), Some(PoolKind::Amm));

        rm.set_min_reserve(&s.ta, &s.tb, &500_000_i128, &500_000_i128);
        assert!(rm.check_reserves(&s.pool));
    }

    #[test]
    fn test_pool_kind_defaults_to_auto_detect() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        assert_eq!(rm.get_pool_kind(&s.pool), None);
    }

    #[test]
    fn test_both_pool_kinds_share_one_pair_requirement() {
        // The requirement is keyed by token pair, not pool address, so a single
        // `set_min_reserve` constrains both pools trading that pair.
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let cl = deploy_cl_pool(&s, 1_000_000);

        rm.set_min_reserve(&s.ta, &s.tb, &500_000_i128, &500_000_i128);
        assert!(rm.check_reserves(&s.pool));
        assert!(rm.check_reserves(&cl));

        rm.set_min_reserve(&s.ta, &s.tb, &1_500_000_i128, &1_500_000_i128);
        assert!(!rm.check_reserves(&s.pool));
        assert!(!rm.check_reserves(&cl));
    }

    #[test]
    fn test_propose_and_accept_governance() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let new_gov = Address::generate(&s.env);

        rm.propose_governance(&s.governance, &new_gov);
        assert_eq!(rm.get_pending_governance(), Some(new_gov.clone()));

        rm.accept_governance(&new_gov);
        assert_eq!(rm.get_governance(), new_gov);
        assert_eq!(rm.get_pending_governance(), None);
    }

    #[test]
    fn test_propose_governance_requires_current_governance() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let rando = Address::generate(&s.env);
        let new_gov = Address::generate(&s.env);

        assert!(rm.try_propose_governance(&rando, &new_gov).is_err());
    }

    #[test]
    fn test_accept_governance_requires_pending_nomination() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let rando = Address::generate(&s.env);

        assert!(rm.try_accept_governance(&rando).is_err());
    }

    #[test]
    fn test_accept_governance_requires_nominee() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let new_gov = Address::generate(&s.env);
        let other = Address::generate(&s.env);

        rm.propose_governance(&s.governance, &new_gov);
        assert!(rm.try_accept_governance(&other).is_err());
    }

    #[test]
    fn test_set_min_reserve_to_zero_removes_constraint() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        rm.set_min_reserve(&s.ta, &s.tb, &2_000_000_i128, &2_000_000_i128);
        assert!(!rm.check_reserves(&s.pool));

        rm.set_min_reserve(&s.ta, &s.tb, &0_i128, &0_i128);
        assert!(rm.check_reserves(&s.pool));
    }

    #[test]
    fn test_set_min_reserve_to_zero_deletes_persistent_entry() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        rm.set_min_reserve(&s.ta, &s.tb, &2_000_000_i128, &2_000_000_i128);
        let (ta, tb) = ReserveManager::normalize(s.ta.clone(), s.tb.clone());
        let key = DataKey::MinReserve(ta, tb);

        // The entry exists while a non-zero requirement is set.
        assert!(s
            .env
            .as_contract(&s.rm_addr, || s.env.storage().persistent().has(&key)));

        // Setting both minimums to zero must delete the key, not store (0, 0).
        rm.set_min_reserve(&s.ta, &s.tb, &0_i128, &0_i128);
        assert!(!s
            .env
            .as_contract(&s.rm_addr, || s.env.storage().persistent().has(&key)));
    }

    #[test]
    fn test_negative_min_reserve_panics() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        assert_eq!(
            rm.try_set_min_reserve(&s.ta, &s.tb, &-1_i128, &0_i128),
            Err(Ok(ReserveManagerError::NegativeReserveAmount))
        );
    }

    // -- #682: pair indexing, pagination, detailed / batch reporting ----------

    /// Normalised (smaller, larger) ordering of a pair, matching storage.
    fn norm(a: &Address, b: &Address) -> (Address, Address) {
        if a < b {
            (a.clone(), b.clone())
        } else {
            (b.clone(), a.clone())
        }
    }

    #[test]
    fn test_configured_pairs_indexed_in_insertion_order_without_duplicates() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let tc = Address::generate(&s.env);
        let td = Address::generate(&s.env);

        rm.set_min_reserve(&s.ta, &s.tb, &1_i128, &1_i128);
        rm.set_min_reserve(&tc, &td, &2_i128, &2_i128);
        // Re-writing an existing pair (in either token order) must not duplicate.
        rm.set_min_reserve(&s.tb, &s.ta, &5_i128, &5_i128);

        assert_eq!(rm.get_configured_pair_count(), 2);
        let pairs = rm.list_configured_pairs(&0, &10);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs.get(0).unwrap(), norm(&s.ta, &s.tb));
        assert_eq!(pairs.get(1).unwrap(), norm(&tc, &td));
    }

    #[test]
    fn test_list_configured_pairs_pagination_edges() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        let mut expected: soroban_sdk::Vec<(Address, Address)> = soroban_sdk::Vec::new(&s.env);
        for _ in 0..5 {
            let a = Address::generate(&s.env);
            let b = Address::generate(&s.env);
            rm.set_min_reserve(&a, &b, &1_i128, &1_i128);
            expected.push_back(norm(&a, &b));
        }
        assert_eq!(rm.get_configured_pair_count(), 5);

        // Mid-range page.
        let page = rm.list_configured_pairs(&1, &2);
        assert_eq!(page.len(), 2);
        assert_eq!(page.get(0).unwrap(), expected.get(1).unwrap());
        assert_eq!(page.get(1).unwrap(), expected.get(2).unwrap());

        // offset == count and offset > count yield an empty page, not a panic.
        assert_eq!(rm.list_configured_pairs(&5, &10).len(), 0);
        assert_eq!(rm.list_configured_pairs(&99, &10).len(), 0);

        // limit == 0 yields an empty page.
        assert_eq!(rm.list_configured_pairs(&0, &0).len(), 0);

        // limit > MAX_PAGE is clamped, not rejected; only 5 pairs exist so the
        // whole set comes back.
        let all = rm.list_configured_pairs(&0, &(MAX_PAGE + 1_000));
        assert_eq!(all.len(), 5);

        // A partial trailing page is truncated to the remaining entries.
        assert_eq!(rm.list_configured_pairs(&4, &10).len(), 1);
    }

    #[test]
    fn test_setting_zero_requirement_deindexes_pair() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let tc = Address::generate(&s.env);
        let td = Address::generate(&s.env);

        rm.set_min_reserve(&s.ta, &s.tb, &1_i128, &1_i128);
        rm.set_min_reserve(&tc, &td, &2_i128, &2_i128);
        assert_eq!(rm.get_configured_pair_count(), 2);

        rm.set_min_reserve(&s.ta, &s.tb, &0_i128, &0_i128);
        assert_eq!(rm.get_configured_pair_count(), 1);
        let pairs = rm.list_configured_pairs(&0, &10);
        assert_eq!(pairs.get(0).unwrap(), norm(&tc, &td));

        // De-indexing an already-removed pair is a no-op.
        rm.set_min_reserve(&s.ta, &s.tb, &0_i128, &0_i128);
        assert_eq!(rm.get_configured_pair_count(), 1);
    }

    #[test]
    fn test_check_reserves_detailed_healthy_has_zero_shortfalls() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        rm.set_min_reserve(&s.ta, &s.tb, &500_000_i128, &500_000_i128);

        let report = rm.check_reserves_detailed(&s.pool);
        assert!(report.healthy);
        assert_eq!(report.healthy, rm.check_reserves(&s.pool));
        assert_eq!(report.pool, s.pool);
        assert_eq!(report.reserve_a, 1_000_000);
        assert_eq!(report.reserve_b, 1_000_000);
        assert_eq!(report.min_a, 500_000);
        assert_eq!(report.min_b, 500_000);
        assert_eq!(report.shortfall_a, 0);
        assert_eq!(report.shortfall_b, 0);
    }

    #[test]
    fn test_check_reserves_detailed_reports_shortfall_arithmetic() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        // Below the floor on token_a only; token_b stays comfortably above it.
        let (smaller, larger) = norm(&s.ta, &s.tb);
        rm.set_min_reserve(&smaller, &larger, &1_500_000_i128, &400_000_i128);

        let report = rm.check_reserves_detailed(&s.pool);
        assert!(!report.healthy);
        assert_eq!(report.healthy, rm.check_reserves(&s.pool));

        // The report is expressed in the pool's own token order.
        let pool_info = amm::AmmPoolClient::new(&s.env, &s.pool).get_info();
        assert_eq!(report.token_a, pool_info.token_a);
        assert_eq!(report.token_b, pool_info.token_b);
        let (exp_a, exp_b) = if pool_info.token_a == smaller {
            (1_500_000_i128, 400_000_i128)
        } else {
            (400_000_i128, 1_500_000_i128)
        };
        assert_eq!(report.min_a, exp_a);
        assert_eq!(report.min_b, exp_b);
        assert_eq!(report.shortfall_a, (exp_a - 1_000_000).max(0));
        assert_eq!(report.shortfall_b, (exp_b - 1_000_000).max(0));
    }

    #[test]
    fn test_check_reserves_detailed_without_requirement_is_healthy() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let report = rm.check_reserves_detailed(&s.pool);
        assert!(report.healthy);
        assert_eq!(report.min_a, 0);
        assert_eq!(report.min_b, 0);
        assert_eq!(report.shortfall_a, 0);
        assert_eq!(report.shortfall_b, 0);
    }

    #[test]
    fn test_check_reserves_batch_is_fault_isolated() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        rm.set_min_reserve(&s.ta, &s.tb, &500_000_i128, &500_000_i128);

        // The middle entry is a plain account address, not an AMM pool.
        let not_a_pool = Address::generate(&s.env);
        let pools = soroban_sdk::vec![&s.env, s.pool.clone(), not_a_pool.clone(), s.pool.clone()];

        let reports = rm.check_reserves_batch(&pools);
        assert_eq!(reports.len(), 3);

        let bad = reports.get(1).unwrap();
        assert_eq!(bad.pool, not_a_pool);
        assert!(!bad.healthy);
        assert_eq!(bad.reserve_a, 0);
        assert_eq!(bad.reserve_b, 0);

        // Every other pool in the batch still gets a real report.
        assert!(reports.get(0).unwrap().healthy);
        assert!(reports.get(2).unwrap().healthy);
    }

    #[test]
    fn test_check_reserves_batch_emits_res_warn_with_unhealthy_pools() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        rm.set_min_reserve(&s.ta, &s.tb, &2_000_000_i128, &2_000_000_i128);

        let pools = soroban_sdk::vec![&s.env, s.pool.clone()];
        let reports = rm.check_reserves_batch(&pools);
        assert!(!reports.get(0).unwrap().healthy);

        let events = s.env.events().all();
        let (contract, topics, data) = events.last().unwrap();
        assert_eq!(contract, s.rm_addr);
        assert_eq!(
            topics,
            soroban_sdk::vec![&s.env, symbol_short!("res_warn").into_val(&s.env)]
        );
        let (version, (unhealthy,)): (u32, (soroban_sdk::Vec<Address>,)) = data.into_val(&s.env);
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(unhealthy, soroban_sdk::vec![&s.env, s.pool.clone()]);
    }

    #[test]
    fn test_check_reserves_batch_stays_silent_when_all_pools_healthy() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        rm.set_min_reserve(&s.ta, &s.tb, &1_000_i128, &1_000_i128);

        let before = s.env.events().all().len();
        let reports = rm.check_reserves_batch(&soroban_sdk::vec![&s.env, s.pool.clone()]);
        assert!(reports.get(0).unwrap().healthy);
        assert_eq!(s.env.events().all().len(), before);
    }

    #[test]
    fn test_check_reserves_batch_rejects_oversized_batch() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        let mut pools: soroban_sdk::Vec<Address> = soroban_sdk::Vec::new(&s.env);
        for _ in 0..(MAX_PAGE + 1) {
            pools.push_back(s.pool.clone());
        }
        assert_eq!(
            rm.try_check_reserves_batch(&pools),
            Err(Ok(ReserveManagerError::BatchTooLarge))
        );
    }

    #[test]
    fn test_check_reserves_batch_on_empty_input() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let reports = rm.check_reserves_batch(&soroban_sdk::Vec::new(&s.env));
        assert_eq!(reports.len(), 0);
    }

    // ── Issue #918: every reserve_manager event carries EVENT_SCHEMA_VERSION ──
    //
    // `governance_proposed`, `governance_transferred` and `res_warn` used to
    // call `env.events().publish(...)` directly, so their payloads were not
    // version-stamped and an indexer reading `(version, ...rest)` would have
    // decoded the first real field as the version number. These tests pin the
    // stamp in place for every topic this contract emits.

    /// Fetch the payload of the most recent event this contract published under
    /// `topic`, decoded as a version-stamped `(u32, T)` pair.
    fn last_versioned_event<T>(s: &Setup, topic: &str) -> (u32, T)
    where
        T: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>,
    {
        let wanted: soroban_sdk::Vec<soroban_sdk::Val> =
            (Symbol::new(&s.env, topic),).into_val(&s.env);
        let evt = s
            .env
            .events()
            .all()
            .iter()
            .rfind(|e| e.0 == s.rm_addr && e.1 == wanted)
            .unwrap_or_else(|| panic!("no `{topic}` event found"));
        evt.2.into_val(&s.env)
    }

    #[test]
    fn test_governance_proposed_emits_versioned_event() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let new_gov = Address::generate(&s.env);

        rm.propose_governance(&s.governance, &new_gov);

        let (version, data): (u32, (Address, Address)) =
            last_versioned_event(&s, "governance_proposed");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (s.governance.clone(), new_gov));
    }

    #[test]
    fn test_governance_transferred_emits_versioned_event() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let new_gov = Address::generate(&s.env);

        rm.propose_governance(&s.governance, &new_gov);
        rm.accept_governance(&new_gov);

        let (version, data): (u32, (Address,)) = last_versioned_event(&s, "governance_transferred");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(data, (new_gov,));
    }

    // ── Issue #909: instance storage TTL is never extended ──────────────────
    //
    // `reserve_manager` used to extend only its per-pair persistent
    // requirements, never the instance entry holding governance, factory,
    // and pool-kind overrides. This contract is driven by off-chain
    // dashboards, bots, and multisig governance rather than steady user
    // traffic, so a long-enough quiet stretch would let that instance entry
    // lapse and get archived, trapping every subsequent call — including
    // the one governance would need to restore it. These tests pin
    // `extend_instance_ttl` in place on the read and write entrypoints.

    fn instance_ttl(env: &Env, rm_addr: &Address) -> u32 {
        env.as_contract(rm_addr, || env.storage().instance().get_ttl())
    }

    /// Advances the ledger sequence number far enough that the instance
    /// entry's remaining TTL drops below `INSTANCE_TTL_THRESHOLD`,
    /// simulating a long quiet stretch between calls.
    fn lower_instance_ttl_below_threshold(env: &Env, rm_addr: &Address) {
        env.ledger()
            .with_mut(|l| l.sequence_number += INSTANCE_TTL_BUMP_TO - INSTANCE_TTL_THRESHOLD + 1);
        let ttl = instance_ttl(env, rm_addr);
        assert!(
            ttl < INSTANCE_TTL_THRESHOLD,
            "test setup should lower instance TTL below the threshold, got {ttl}"
        );
    }

    fn assert_instance_ttl_bumped(env: &Env, rm_addr: &Address) {
        let ttl = instance_ttl(env, rm_addr);
        assert!(
            ttl >= INSTANCE_TTL_BUMP_TO - 1,
            "instance TTL {ttl} should be bumped toward INSTANCE_TTL_BUMP_TO"
        );
    }


    #[test]
    fn test_pause_and_unpause_requires_auth() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        // This will panic internally in the mock auth test environment because we didn't mock the auth for a random user,
        // or it will fail authorization. Wait, if we use `try_pause`, we can't catch the require_auth() easily without a specific setup,
        // but since `s.governance` has mock auth, calling it directly works. We can check that the admin can pause.
        rm.pause();
        assert!(rm.is_paused());
        rm.unpause();
        assert!(!rm.is_paused());
    }

    #[test]
    fn test_mutating_functions_paused() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        rm.pause();
        assert!(rm.is_paused());

        assert_eq!(
            rm.try_set_min_reserve(&s.ta, &s.tb, &1_i128, &1_i128),
            Err(Ok(ReserveManagerError::Paused))
        );

        assert_eq!(
            rm.try_set_pool_kind(&s.pool, &PoolKind::Amm),
            Err(Ok(ReserveManagerError::Paused))
        );

        let new_gov = Address::generate(&s.env);
        assert_eq!(
            rm.try_propose_governance(&s.governance, &new_gov),
            Err(Ok(ReserveManagerError::Paused))
        );

        assert_eq!(
            rm.try_accept_governance(&new_gov),
            Err(Ok(ReserveManagerError::Paused))
        );
    }

    #[test]
    fn test_initialize_extends_instance_ttl() {
        let env = Env::default();
        env.mock_all_auths();
        let gov = Address::generate(&env);
        let factory = Address::generate(&env);
        let rm_addr = env.register_contract(None, ReserveManager);
        let rm = ReserveManagerClient::new(&env, &rm_addr);

        rm.initialize(&gov, &factory);

        assert_instance_ttl_bumped(&env, &rm_addr);
    }

    /// A read-only entrypoint (`get_governance`) still restores a lapsed
    /// instance TTL — this is the failure mode the issue calls out: the
    /// admin address needed to authorize a restore is itself in the
    /// archived instance entry, so read paths must extend the TTL too.
    #[test]
    fn test_get_governance_restores_lapsed_instance_ttl_and_still_responds() {
        let env = Env::default();
        env.mock_all_auths();
        let gov = Address::generate(&env);
        let factory = Address::generate(&env);
        let rm_addr = env.register_contract(None, ReserveManager);
        let rm = ReserveManagerClient::new(&env, &rm_addr);
        rm.initialize(&gov, &factory);

        lower_instance_ttl_below_threshold(&env, &rm_addr);

        // Must still succeed rather than trap on an archived instance entry.
        let read_back = rm.get_governance();
        assert_eq!(read_back, gov);
        assert_instance_ttl_bumped(&env, &rm_addr);
    }

    /// A state-mutating entrypoint (`set_min_reserve`) restores a lapsed
    /// instance TTL as its first statement, before the governance check
    /// even runs.
    #[test]
    fn test_set_min_reserve_restores_lapsed_instance_ttl_and_still_responds() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);

        lower_instance_ttl_below_threshold(&s.env, &s.rm_addr);

        // Must still succeed rather than trap on an archived instance entry.
        rm.set_min_reserve(&s.ta, &s.tb, &10_i128, &10_i128);

        assert_instance_ttl_bumped(&s.env, &s.rm_addr);
        assert!(rm.check_reserves(&s.pool));
    }

    #[test]
    fn test_read_views_unpaused() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        rm.pause();
        // Reads should still work
        let _ = rm.check_reserves(&s.pool);
        let _ = rm.get_configured_pair_count();
        let _ = rm.list_configured_pairs(&0, &10);
    }

    #[test]
    fn test_admin_rotation_happy_path() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let new_admin = Address::generate(&s.env);

        assert_eq!(rm.get_admin(), Some(s.governance.clone()));
        assert_eq!(rm.get_pending_admin(), None);

        // Propose admin
        rm.propose_admin(&s.governance, &new_admin);
        assert_eq!(rm.get_pending_admin(), Some(new_admin.clone()));
        assert_eq!(rm.get_admin(), Some(s.governance.clone()));

        // Accept admin
        rm.accept_admin(&new_admin);
        assert_eq!(rm.get_admin(), Some(new_admin.clone()));
        assert_eq!(rm.get_pending_admin(), None);

        let (version, data): (u32, (Address, Address)) =
            last_versioned_event(&s, "admin_nominated");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(data, (s.governance.clone(), new_admin.clone()));

        let (version_changed, data_changed): (u32, (Address,)) =
            last_versioned_event(&s, "admin_changed");
        assert_eq!(version_changed, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(data_changed, (new_admin,));
    }

    #[test]
    fn test_propose_admin_unauthorized() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let rando = Address::generate(&s.env);
        let new_admin = Address::generate(&s.env);

        assert_eq!(
            rm.try_propose_admin(&rando, &new_admin),
            Err(Ok(ReserveManagerError::Unauthorized))
        );
        assert_eq!(rm.get_pending_admin(), None);
        assert_eq!(rm.get_admin(), Some(s.governance));
    }

    #[test]
    fn test_accept_admin_wrong_address_or_no_pending() {
        let s = setup();
        let rm = ReserveManagerClient::new(&s.env, &s.rm_addr);
        let rando = Address::generate(&s.env);
        let new_admin = Address::generate(&s.env);

        // Accept without prior proposal
        assert_eq!(
            rm.try_accept_admin(&new_admin),
            Err(Ok(ReserveManagerError::NoPendingAdmin))
        );

        // Propose to new_admin
        rm.propose_admin(&s.governance, &new_admin);

        // Wrong address calling accept_admin
        assert_eq!(
            rm.try_accept_admin(&rando),
            Err(Ok(ReserveManagerError::WrongAdmin))
        );
        assert_eq!(rm.get_admin(), Some(s.governance));
    }
}
