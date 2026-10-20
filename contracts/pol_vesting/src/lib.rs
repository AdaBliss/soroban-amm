//! Protocol-Owned Liquidity (POL) Vesting Contract
//!
//! Governance can create time-based vesting schedules for LP tokens so that
//! protocol-owned liquidity cannot be withdrawn in a single governance vote.
//! Tokens vest linearly between `cliff_ledger` and `end_ledger`.

#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Symbol, Vec,
};

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum VestingError {
    AlreadyInitialized = 1,
    NotGovernance = 2,
    VestingNotFound = 3,
    VestingAlreadyExists = 4,
    NothingToRelease = 5,
    InvalidSchedule = 6,
    NotBeneficiary = 7,
    NoPendingGovernance = 8,
    NoPendingTreasury = 9,
    NotTreasury = 10,
    /// The contract holds fewer uncommitted tokens than the schedule requires.
    InsufficientFunding = 11,
    /// A beneficiary already holds the maximum number of live schedules.
    TooManySchedules = 12,
}

// ── Constants ─────────────────────────────────────────────────────────────────

const MIN_TTL: u32 = 241_920; // ~14 days (at 5s per ledger)
const BUMP_TO: u32 = 3_110_400; // ~180 days (at 5s per ledger)

/// Maximum number of live schedules a single beneficiary may hold. Bounds the
/// per-beneficiary id list and the amount of work a caller can force on
/// `list_schedules`.
const MAX_SCHEDULES_PER_BENEFICIARY: u32 = 100;
/// Hard ceiling on `list_schedules`'s `limit`, so a caller cannot ask for an
/// unbounded response.
const MAX_PAGE: u32 = 50;

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    /// Governance contract address (the only caller allowed to create/revoke).
    Governance,
    /// Pending governance nominee for two-step governance rotation.
    PendingGovernance,
    /// Treasury address — receives tokens on revocation.
    Treasury,
    /// Pending treasury nominee for two-step treasury rotation.
    PendingTreasury,
    /// Next schedule id for a beneficiary.
    NextScheduleId(Address),
    /// Per-beneficiary vesting schedule keyed by beneficiary and schedule id.
    Vesting(Address, u32),
    /// LP tokens committed to live schedules, keyed by LP token address.
    Committed(Address),
    /// Live schedule ids for a beneficiary, in creation order. Maintained by
    /// `create_vesting`, `change_beneficiary` and `revoke_vesting` so the exact
    /// set of schedules can be enumerated without gaps.
    SchedulesOf(Address),
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PolVesting {
    pub schedule_id: u32,
    pub lp_token: Address,
    pub pool: Address,
    pub total: i128,
    pub released: i128,
    pub start_ledger: u32,
    pub cliff_ledger: u32,
    pub end_ledger: u32,
    pub beneficiary: Address,
    pub revoked: bool,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct PolVestingContract;

#[contractimpl]
impl PolVestingContract {
    /// One-time setup. `governance` is the only address allowed to create or
    /// revoke schedules. `treasury` receives tokens when a schedule is revoked.
    pub fn initialize(
        env: Env,
        governance: Address,
        treasury: Address,
    ) -> Result<(), VestingError> {
        Self::extend_ttl(&env);
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(VestingError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        Ok(())
    }

    /// Nominate a new governance address.
    ///
    /// Requires current governance authorization. The nominee must call
    /// `accept_governance` to complete the handover.
    pub fn propose_governance(
        env: Env,
        current_governance: Address,
        new_governance: Address,
    ) -> Result<(), VestingError> {
        Self::extend_ttl(&env);
        current_governance.require_auth();
        Self::require_governance(&env, &current_governance)?;

        env.storage()
            .instance()
            .set(&DataKey::PendingGovernance, &Some(new_governance.clone()));

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "governance_proposed"),),
            (current_governance, new_governance)
        );
        Ok(())
    }

    /// Accept a pending governance nomination.
    ///
    /// Only the nominated address can accept. On success, governance is updated,
    /// the pending nominee is cleared, and a transfer event is emitted.
    pub fn accept_governance(env: Env, new_governance: Address) -> Result<(), VestingError> {
        Self::extend_ttl(&env);
        let pending: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PendingGovernance)
            .unwrap_or(None);
        let nominee = pending.ok_or(VestingError::NoPendingGovernance)?;

        if new_governance != nominee {
            return Err(VestingError::NotGovernance);
        }
        new_governance.require_auth();

        let old_governance: Address = env.storage().instance().get(&DataKey::Governance).unwrap();
        env.storage()
            .instance()
            .set(&DataKey::Governance, &new_governance);
        env.storage()
            .instance()
            .set(&DataKey::PendingGovernance, &Option::<Address>::None);

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "governance_transferred"),),
            (old_governance, new_governance)
        );
        Ok(())
    }

    /// Return the active governance address.
    pub fn get_governance(env: Env) -> Address {
        Self::extend_ttl(&env);
        env.storage().instance().get(&DataKey::Governance).unwrap()
    }

    /// Return the pending governance nominee, if any.
    pub fn get_pending_governance(env: Env) -> Option<Address> {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::PendingGovernance)
            .unwrap_or(None)
    }

    /// Nominate a new treasury address.
    ///
    /// Requires governance authorization. The nominee must call
    /// `accept_treasury` to complete the handover.
    pub fn propose_treasury(
        env: Env,
        governance: Address,
        new_treasury: Address,
    ) -> Result<(), VestingError> {
        Self::extend_ttl(&env);
        governance.require_auth();
        Self::require_governance(&env, &governance)?;

        env.storage()
            .instance()
            .set(&DataKey::PendingTreasury, &Some(new_treasury.clone()));

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "treasury_proposed"),),
            (governance, new_treasury)
        );
        Ok(())
    }

    /// Accept a pending treasury nomination.
    ///
    /// Only the nominated address can accept. On success, treasury is updated,
    /// the pending nominee is cleared, and an event is emitted.
    pub fn accept_treasury(env: Env, new_treasury: Address) -> Result<(), VestingError> {
        Self::extend_ttl(&env);
        let pending: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PendingTreasury)
            .unwrap_or(None);
        let nominee = pending.ok_or(VestingError::NoPendingTreasury)?;

        if new_treasury != nominee {
            return Err(VestingError::NotTreasury);
        }
        new_treasury.require_auth();

        let old_treasury: Address = env.storage().instance().get(&DataKey::Treasury).unwrap();
        env.storage()
            .instance()
            .set(&DataKey::Treasury, &new_treasury);
        env.storage()
            .instance()
            .set(&DataKey::PendingTreasury, &Option::<Address>::None);

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "treasury_transferred"),),
            (old_treasury, new_treasury)
        );
        Ok(())
    }

    /// Return the active treasury address.
    pub fn get_treasury(env: Env) -> Address {
        Self::extend_ttl(&env);
        env.storage().instance().get(&DataKey::Treasury).unwrap()
    }

    /// Return the pending treasury nominee, if any.
    pub fn get_pending_treasury(env: Env) -> Option<Address> {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::PendingTreasury)
            .unwrap_or(None)
    }

    /// Governance creates a vesting schedule for `beneficiary`.
    ///
    /// The caller must be the governance contract and must have already
    /// transferred `total` LP tokens to this contract before calling.
    ///
    /// - `cliff_ledger` must be >= `start_ledger`
    /// - `end_ledger` must be > `cliff_ledger`
    #[allow(clippy::too_many_arguments)]
    pub fn create_vesting(
        env: Env,
        governance: Address,
        beneficiary: Address,
        lp_token: Address,
        pool: Address,
        total: i128,
        start_ledger: u32,
        cliff_ledger: u32,
        end_ledger: u32,
    ) -> Result<u32, VestingError> {
        Self::extend_ttl(&env);
        governance.require_auth();
        Self::require_governance(&env, &governance)?;

        if cliff_ledger < start_ledger || end_ledger <= cliff_ledger || total <= 0 {
            return Err(VestingError::InvalidSchedule);
        }

        // A beneficiary may only accumulate a bounded number of live schedules.
        // Ids come from a per-beneficiary counter, so the cap is measured on the
        // live id list rather than on the (monotonically rising) counter.
        let list_key = DataKey::SchedulesOf(beneficiary.clone());
        let mut schedule_ids: Vec<u32> = env
            .storage()
            .persistent()
            .get(&list_key)
            .unwrap_or(Vec::new(&env));
        if schedule_ids.len() >= MAX_SCHEDULES_PER_BENEFICIARY {
            return Err(VestingError::TooManySchedules);
        }

        let next_id_key = DataKey::NextScheduleId(beneficiary.clone());
        let schedule_id: u32 = env.storage().persistent().get(&next_id_key).unwrap_or(0);
        let key = DataKey::Vesting(beneficiary.clone(), schedule_id);
        if env.storage().persistent().has(&key) {
            return Err(VestingError::VestingAlreadyExists);
        }

        // Creating a schedule commits `total` LP tokens of `lp_token`. Refuse to
        // over-commit the shared contract balance: tokens already backing other
        // schedules must stay untouchable by any new schedule.
        let committed = Self::read_committed(&env, &lp_token);
        let uncommitted = token::Client::new(&env, &lp_token)
            .balance(&env.current_contract_address())
            .checked_sub(committed)
            .unwrap_or(0);
        if uncommitted < total {
            return Err(VestingError::InsufficientFunding);
        }

        let schedule = PolVesting {
            schedule_id,
            lp_token,
            pool,
            total,
            released: 0,
            start_ledger,
            cliff_ledger,
            end_ledger,
            beneficiary: beneficiary.clone(),
            revoked: false,
        };
        env.storage().persistent().set(&key, &schedule);
        env.storage()
            .persistent()
            .extend_ttl(&key, MIN_TTL, BUMP_TO);
        env.storage()
            .persistent()
            .set(&next_id_key, &(schedule_id + 1));
        env.storage()
            .persistent()
            .extend_ttl(&next_id_key, MIN_TTL, BUMP_TO);

        // Record the commitment and append the schedule to the beneficiary's
        // live-id list, so the exact set of schedules can be enumerated later.
        let committed_key = DataKey::Committed(schedule.lp_token.clone());
        env.storage()
            .persistent()
            .set(&committed_key, &(committed + total));
        env.storage()
            .persistent()
            .extend_ttl(&committed_key, MIN_TTL, BUMP_TO);
        schedule_ids.push_back(schedule_id);
        env.storage().persistent().set(&list_key, &schedule_ids);
        env.storage()
            .persistent()
            .extend_ttl(&list_key, MIN_TTL, BUMP_TO);

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "vesting_created"),),
            (
                beneficiary,
                schedule_id,
                total,
                start_ledger,
                cliff_ledger,
                end_ledger,
            )
        );
        Ok(schedule_id)
    }

    /// Release all currently vested (but unreleased) LP tokens to the beneficiary.
    pub fn release(env: Env, beneficiary: Address, schedule_id: u32) -> Result<i128, VestingError> {
        Self::extend_ttl(&env);
        beneficiary.require_auth();

        let key = DataKey::Vesting(beneficiary.clone(), schedule_id);
        let mut schedule: PolVesting = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(VestingError::VestingNotFound)?;

        env.storage()
            .persistent()
            .extend_ttl(&key, MIN_TTL, BUMP_TO);

        if schedule.beneficiary != beneficiary {
            return Err(VestingError::NotBeneficiary);
        }

        let current_ledger = env.ledger().sequence();
        let releasable = Self::vested_at(&schedule, current_ledger) - schedule.released;
        if releasable <= 0 {
            return Err(VestingError::NothingToRelease);
        }

        schedule.released += releasable;
        env.storage().persistent().set(&key, &schedule);

        // Releasing tokens pays them out and therefore un-commits them.
        //
        // `i128` subtraction only traps on overflow past the type's range, not
        // on crossing zero, so a plain `-` (or `saturating_sub`, which behaves
        // identically here) would silently let `committed` go negative if this
        // invariant were ever violated by a bug elsewhere — and a negative
        // `committed` would then *inflate* `create_vesting`'s `balance -
        // committed` affordability check instead of being caught. Trap loudly
        // instead.
        let committed_key = DataKey::Committed(schedule.lp_token.clone());
        let committed = Self::read_committed(&env, &schedule.lp_token);
        let new_committed = committed
            .checked_sub(releasable)
            .filter(|c| *c >= 0)
            .expect("pol_vesting: committed underflow - invariant violated");
        env.storage()
            .persistent()
            .set(&committed_key, &new_committed);
        env.storage()
            .persistent()
            .extend_ttl(&committed_key, MIN_TTL, BUMP_TO);

        token::Client::new(&env, &schedule.lp_token).transfer(
            &env.current_contract_address(),
            &beneficiary,
            &releasable,
        );

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "released"),),
            (beneficiary, schedule_id, releasable)
        );
        Ok(releasable)
    }

    /// Read the vesting schedule for a beneficiary.
    pub fn get_vesting(
        env: Env,
        beneficiary: Address,
        schedule_id: u32,
    ) -> Result<PolVesting, VestingError> {
        Self::extend_ttl(&env);
        env.storage()
            .persistent()
            .get(&DataKey::Vesting(beneficiary, schedule_id))
            .ok_or(VestingError::VestingNotFound)
    }

    /// Total LP tokens currently committed to live schedules for `lp_token`.
    ///
    /// This is the amount the contract must keep in reserve: the sum of every
    /// schedule's unreleased remainder on that token. The difference between the
    /// contract's token balance and this figure is what a new schedule may draw.
    pub fn committed(env: Env, lp_token: Address) -> i128 {
        Self::extend_ttl(&env);
        Self::read_committed(&env, &lp_token)
    }

    /// Total LP tokens vested to date for a schedule, ignoring releases.
    ///
    /// A pure preview of `release`'s accrual curve; requires no authorization.
    pub fn vested_amount(
        env: Env,
        beneficiary: Address,
        schedule_id: u32,
    ) -> Result<i128, VestingError> {
        Self::extend_ttl(&env);
        let schedule: PolVesting = env
            .storage()
            .persistent()
            .get(&DataKey::Vesting(beneficiary, schedule_id))
            .ok_or(VestingError::VestingNotFound)?;
        Ok(Self::vested_at(&schedule, env.ledger().sequence()))
    }

    /// Amount a `release` call would pay right now for a schedule.
    ///
    /// Equals `vested_amount - released`, clamped at zero. A pure preview that
    /// requires no authorization.
    pub fn releasable_amount(
        env: Env,
        beneficiary: Address,
        schedule_id: u32,
    ) -> Result<i128, VestingError> {
        Self::extend_ttl(&env);
        let schedule: PolVesting = env
            .storage()
            .persistent()
            .get(&DataKey::Vesting(beneficiary, schedule_id))
            .ok_or(VestingError::VestingNotFound)?;
        let vested = Self::vested_at(&schedule, env.ledger().sequence());
        Ok((vested - schedule.released).max(0))
    }

    /// Number of live schedules held by a beneficiary.
    pub fn schedule_count(env: Env, beneficiary: Address) -> u32 {
        Self::extend_ttl(&env);
        let key = DataKey::SchedulesOf(beneficiary);
        let ids: Vec<u32> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(Vec::new(&env));
        if env.storage().persistent().has(&key) {
            env.storage()
                .persistent()
                .extend_ttl(&key, MIN_TTL, BUMP_TO);
        }
        ids.len()
    }

    /// Page through a beneficiary's live schedules.
    ///
    /// `limit` is clamped to `MAX_PAGE`; `offset` beyond the end yields an empty
    /// vector. Revoked and reassigned schedules are absent, so the pages contain
    /// no gaps.
    pub fn list_schedules(
        env: Env,
        beneficiary: Address,
        offset: u32,
        limit: u32,
    ) -> Vec<PolVesting> {
        Self::extend_ttl(&env);
        let list_key = DataKey::SchedulesOf(beneficiary.clone());
        let ids: Vec<u32> = env
            .storage()
            .persistent()
            .get(&list_key)
            .unwrap_or(Vec::new(&env));
        if env.storage().persistent().has(&list_key) {
            env.storage()
                .persistent()
                .extend_ttl(&list_key, MIN_TTL, BUMP_TO);
        }

        let mut schedules: Vec<PolVesting> = Vec::new(&env);
        let count = ids.len();
        if offset >= count || limit == 0 {
            return schedules;
        }
        let end = core::cmp::min(offset.saturating_add(limit.min(MAX_PAGE)), count);
        let mut i = offset;
        while i < end {
            let schedule_id = ids.get(i).unwrap();
            if let Some(schedule) = env
                .storage()
                .persistent()
                .get::<DataKey, PolVesting>(&DataKey::Vesting(beneficiary.clone(), schedule_id))
            {
                schedules.push_back(schedule);
            }
            i += 1;
        }
        schedules
    }

    /// Governance can reassign a vesting schedule to a new beneficiary.
    ///
    /// The schedule is transferred intact, preserving the total, released,
    /// and vesting timeline. A new schedule ID is generated for the new beneficiary.
    pub fn change_beneficiary(
        env: Env,
        governance: Address,
        old_beneficiary: Address,
        old_schedule_id: u32,
        new_beneficiary: Address,
    ) -> Result<u32, VestingError> {
        Self::extend_ttl(&env);
        governance.require_auth();
        Self::require_governance(&env, &governance)?;

        let old_key = DataKey::Vesting(old_beneficiary.clone(), old_schedule_id);
        let mut schedule: PolVesting = env
            .storage()
            .persistent()
            .get(&old_key)
            .ok_or(VestingError::VestingNotFound)?;

        if schedule.revoked {
            return Err(VestingError::InvalidSchedule);
        }

        // Validate the destination before mutating anything: returning `Err`
        // does not roll storage back, so a rejected move must leave the old
        // schedule and both id lists untouched.
        let new_list_key = DataKey::SchedulesOf(new_beneficiary.clone());
        let new_ids: Vec<u32> = env
            .storage()
            .persistent()
            .get(&new_list_key)
            .unwrap_or(Vec::new(&env));
        if old_beneficiary != new_beneficiary && new_ids.len() >= MAX_SCHEDULES_PER_BENEFICIARY {
            return Err(VestingError::TooManySchedules);
        }

        let next_id_key = DataKey::NextScheduleId(new_beneficiary.clone());
        let new_schedule_id: u32 = env.storage().persistent().get(&next_id_key).unwrap_or(0);
        let new_key = DataKey::Vesting(new_beneficiary.clone(), new_schedule_id);
        if env.storage().persistent().has(&new_key) {
            return Err(VestingError::VestingAlreadyExists);
        }

        env.storage().persistent().remove(&old_key);
        Self::remove_schedule_id(&env, &old_beneficiary, old_schedule_id);

        schedule.schedule_id = new_schedule_id;
        schedule.beneficiary = new_beneficiary.clone();

        env.storage().persistent().set(&new_key, &schedule);
        env.storage()
            .persistent()
            .extend_ttl(&new_key, MIN_TTL, BUMP_TO);
        env.storage()
            .persistent()
            .set(&next_id_key, &(new_schedule_id + 1));
        env.storage()
            .persistent()
            .extend_ttl(&next_id_key, MIN_TTL, BUMP_TO);

        // Re-read the destination list: when source and destination are the same
        // beneficiary, `remove_schedule_id` above just changed it, so appending
        // to the cached copy would resurrect the removed id.
        let mut dest_ids: Vec<u32> = env
            .storage()
            .persistent()
            .get(&new_list_key)
            .unwrap_or(Vec::new(&env));
        dest_ids.push_back(new_schedule_id);
        env.storage().persistent().set(&new_list_key, &dest_ids);
        env.storage()
            .persistent()
            .extend_ttl(&new_list_key, MIN_TTL, BUMP_TO);

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "beneficiary_changed"),),
            (
                old_beneficiary,
                old_schedule_id,
                new_beneficiary.clone(),
                new_schedule_id,
            )
        );
        Ok(new_schedule_id)
    }

    /// Governance cancels a vesting schedule. The schedule's vested amount is
    /// frozen, unvested tokens are returned to the treasury, and the beneficiary
    /// can claim the vested remainder via `release`.
    pub fn revoke_vesting(
        env: Env,
        governance: Address,
        beneficiary: Address,
        schedule_id: u32,
    ) -> Result<(), VestingError> {
        Self::extend_ttl(&env);
        governance.require_auth();
        Self::require_governance(&env, &governance)?;

        let key = DataKey::Vesting(beneficiary.clone(), schedule_id);
        let mut schedule: PolVesting = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(VestingError::VestingNotFound)?;

        if schedule.revoked {
            return Err(VestingError::InvalidSchedule);
        }

        env.storage()
            .persistent()
            .extend_ttl(&key, MIN_TTL, BUMP_TO);

        let current_ledger = env.ledger().sequence();
        let vested = Self::vested_at(&schedule, current_ledger);
        // Under the new model, revoke_vesting NEVER transfers directly to the
        // beneficiary. The beneficiary claims vested-but-unreleased tokens
        // later by calling `release`.
        let to_beneficiary = vested - schedule.released;
        let to_treasury = schedule.total - vested;

        let lp = token::Client::new(&env, &schedule.lp_token);
        let contract_addr = env.current_contract_address();

        if to_treasury > 0 {
            let treasury: Address = env.storage().instance().get(&DataKey::Treasury).unwrap();
            lp.transfer(&contract_addr, &treasury, &to_treasury);
        }

        // Only the treasury refund leaves the commitment book upon revoke,
        // because the frozen remainder is still owed to the beneficiary.
        if to_treasury > 0 {
            let committed_key = DataKey::Committed(schedule.lp_token.clone());
            let committed = Self::read_committed(&env, &schedule.lp_token);
            let new_committed = committed
                .checked_sub(to_treasury)
                .filter(|c| *c >= 0)
                .expect("pol_vesting: committed underflow - invariant violated");
            env.storage()
                .persistent()
                .set(&committed_key, &new_committed);
            env.storage()
                .persistent()
                .extend_ttl(&committed_key, MIN_TTL, BUMP_TO);
        }

        schedule.total = vested;
        schedule.revoked = true;

        env.storage().persistent().set(&key, &schedule);
        Self::remove_schedule_id(&env, &schedule.beneficiary, schedule_id);

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "vesting_revoked"),),
            (beneficiary, schedule_id, to_beneficiary, to_treasury)
        );
        Ok(())
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Extend the contract's **instance** storage TTL.
    ///
    /// The instance entry holds the executable plus every `storage().instance()`
    /// value (governance, treasury and the two pending nominees). Vesting
    /// schedules are long-lived by construction — a year-long schedule is
    /// precisely the case where the instance entry lapses from disuse between
    /// claims — so every entrypoint, including read-only ones, extends it. If
    /// the entry is archived a returning beneficiary finds the contract
    /// unreachable and cannot release vested tokens until it is restored
    /// (issue #908).
    ///
    /// Reuses the module-level `MIN_TTL` / `BUMP_TO` already applied to the
    /// persistent schedule entries, so instance and persistent state age at one
    /// horizon. At the ~5s/ledger Stellar cadence:
    /// - `MIN_TTL` = 241_920 ledgers ≈ 14 days: only rewrite when less than two
    ///   weeks of life remains.
    /// - `BUMP_TO` = 3_110_400 ledgers ≈ 180 days: renew toward the maximum
    ///   persistent rent window, so one call keeps the contract live for months.
    fn extend_ttl(env: &Env) {
        env.storage().instance().extend_ttl(MIN_TTL, BUMP_TO);
    }

    fn require_governance(env: &Env, caller: &Address) -> Result<(), VestingError> {
        let gov: Address = env.storage().instance().get(&DataKey::Governance).unwrap();
        if gov != *caller {
            return Err(VestingError::NotGovernance);
        }
        Ok(())
    }

    /// LP tokens committed to live schedules for `lp_token` (0 when none).
    ///
    /// Bumps the entry's TTL on read. `Committed` is the accounting invariant
    /// that keeps schedules on a shared LP token from over-committing it, so it
    /// must outlive the balance it describes: if it lapsed while tokens were
    /// still committed, the next `create_vesting` would read 0 and re-admit the
    /// exact over-commitment #1044 removed. Reading is the only way a caller
    /// touches this entry, so the renewal has to happen here.
    fn read_committed(env: &Env, lp_token: &Address) -> i128 {
        let key = DataKey::Committed(lp_token.clone());
        match env.storage().persistent().get::<DataKey, i128>(&key) {
            Some(committed) => {
                env.storage()
                    .persistent()
                    .extend_ttl(&key, MIN_TTL, BUMP_TO);
                committed
            }
            None => 0,
        }
    }

    /// Remove `schedule_id` from a beneficiary's live-id list, if present.
    fn remove_schedule_id(env: &Env, beneficiary: &Address, schedule_id: u32) {
        let key = DataKey::SchedulesOf(beneficiary.clone());
        let ids: Vec<u32> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(Vec::new(env));
        let mut kept: Vec<u32> = Vec::new(env);
        for i in 0..ids.len() {
            let id = ids.get(i).unwrap();
            if id != schedule_id {
                kept.push_back(id);
            }
        }
        env.storage().persistent().set(&key, &kept);
        env.storage()
            .persistent()
            .extend_ttl(&key, MIN_TTL, BUMP_TO);
    }

    /// Linear vesting between `cliff_ledger` and `end_ledger`; 0 before the
    /// cliff; `total` at or after `end_ledger`.
    ///
    /// Accrual starts at the cliff, not at `start_ledger`. Measuring elapsed
    /// time from `start_ledger` (while gating release on the cliff) would make
    /// the vested amount jump discontinuously to `total * (cliff - start) /
    /// (end - start)` the instant the cliff is reached — a lump-sum unlock of
    /// protocol-owned liquidity that contradicts the documented schedule and
    /// the "cannot be withdrawn in a single step" guarantee.
    fn vested_at(schedule: &PolVesting, current_ledger: u32) -> i128 {
        if schedule.revoked {
            return schedule.total;
        }
        if current_ledger < schedule.cliff_ledger {
            return 0;
        }
        if current_ledger >= schedule.end_ledger {
            return schedule.total;
        }
        // `create_vesting` enforces `end_ledger > cliff_ledger`, so `duration`
        // is always positive.
        let elapsed = (current_ledger - schedule.cliff_ledger) as i128;
        let duration = (schedule.end_ledger - schedule.cliff_ledger) as i128;
        schedule.total * elapsed / duration
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{storage::Instance as _, Address as _, Ledger},
        token::{StellarAssetClient, TokenClient},
        Env,
    };

    struct Setup {
        env: Env,
        contract_id: Address,
        governance: Address,
        treasury: Address,
        beneficiary: Address,
        lp_token: Address,
        pool: Address,
    }

    fn setup() -> Setup {
        setup_funded(1_000_000)
    }

    /// Setup with an explicit starting LP balance on the vesting contract.
    fn setup_funded(amount: i128) -> Setup {
        let env = Env::default();
        env.mock_all_auths();

        let governance = Address::generate(&env);
        let treasury = Address::generate(&env);
        let beneficiary = Address::generate(&env);
        let pool = Address::generate(&env);

        // Use the built-in Stellar asset contract as a SEP-41 token.
        let lp_token = env
            .register_stellar_asset_contract_v2(governance.clone())
            .address();

        let contract_id = env.register_contract(None, PolVestingContract);
        let client = PolVestingContractClient::new(&env, &contract_id);
        client.initialize(&governance, &treasury);

        if amount > 0 {
            StellarAssetClient::new(&env, &lp_token).mint(&contract_id, &amount);
        }

        Setup {
            env,
            contract_id,
            governance,
            treasury,
            beneficiary,
            lp_token,
            pool,
        }
    }

    fn mint_to_contract(s: &Setup, amount: i128) {
        StellarAssetClient::new(&s.env, &s.lp_token).mint(&s.contract_id, &amount);
    }

    fn create_schedule(s: &Setup, start: u32, cliff: u32, end: u32) -> u32 {
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        client.create_vesting(
            &s.governance,
            &s.beneficiary,
            &s.lp_token,
            &s.pool,
            &1_000_000,
            &start,
            &cliff,
            &end,
        )
    }

    #[test]
    fn test_cliff_enforcement() {
        let s = setup();
        s.env.ledger().set_sequence_number(100);
        let schedule_id = create_schedule(&s, 100, 200, 400);

        // Before cliff — nothing to release.
        s.env.ledger().set_sequence_number(150);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let err = client
            .try_release(&s.beneficiary, &schedule_id)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, VestingError::NothingToRelease);
    }

    #[test]
    fn test_linear_vesting() {
        let s = setup();
        // start=0, cliff=0, end=1000 → fully linear from ledger 0
        let schedule_id = create_schedule(&s, 0, 0, 1000);

        s.env.ledger().set_sequence_number(500);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let released = client.release(&s.beneficiary, &schedule_id);
        assert_eq!(released, 500_000); // 50% of 1_000_000

        let schedule = client.get_vesting(&s.beneficiary, &schedule_id);
        assert_eq!(schedule.released, 500_000);
    }

    #[test]
    fn test_linear_vesting_from_cliff_not_start() {
        let s = setup();
        // start=100, cliff=200, end=400: a 100-ledger gap between start and
        // cliff. Vesting must accrue linearly over [cliff, end], not [start, end].
        let schedule_id = create_schedule(&s, 100, 200, 400);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);

        // At the cliff exactly: nothing has accrued yet — no lump-sum unlock.
        s.env.ledger().set_sequence_number(200);
        let err = client
            .try_release(&s.beneficiary, &schedule_id)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, VestingError::NothingToRelease);

        // Halfway between cliff (200) and end (400): 50% vested.
        s.env.ledger().set_sequence_number(300);
        let released = client.release(&s.beneficiary, &schedule_id);
        assert_eq!(released, 500_000);

        // At end: the remainder becomes releasable, totalling `total`.
        s.env.ledger().set_sequence_number(400);
        let released = client.release(&s.beneficiary, &schedule_id);
        assert_eq!(released, 500_000);
        assert_eq!(
            client.get_vesting(&s.beneficiary, &schedule_id).released,
            1_000_000
        );
    }

    #[test]
    fn test_full_release() {
        let s = setup();
        let schedule_id = create_schedule(&s, 0, 0, 1000);

        s.env.ledger().set_sequence_number(1000);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let released = client.release(&s.beneficiary, &schedule_id);
        assert_eq!(released, 1_000_000);

        // Nothing left to release.
        let err = client
            .try_release(&s.beneficiary, &schedule_id)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, VestingError::NothingToRelease);
    }

    #[test]
    fn test_revoke() {
        let s = setup();
        // start=0, cliff=0, end=1000; revoke at ledger 250 → 25% vested
        let schedule_id = create_schedule(&s, 0, 0, 1000);

        s.env.ledger().set_sequence_number(250);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        client.revoke_vesting(&s.governance, &s.beneficiary, &schedule_id);

        let lp = TokenClient::new(&s.env, &s.lp_token);
        // Revoke never transfers directly to beneficiary; 75% goes to treasury
        assert_eq!(lp.balance(&s.beneficiary), 0);
        assert_eq!(lp.balance(&s.treasury), 750_000);

        // Schedule is frozen, marked revoked, and remains queryable.
        let sched = client.get_vesting(&s.beneficiary, &schedule_id);
        assert!(sched.revoked);
        assert_eq!(sched.total, 250_000);
        assert_eq!(sched.released, 0);

        // Beneficiary claims the remaining vested tokens via release.
        assert_eq!(client.release(&s.beneficiary, &schedule_id), 250_000);
        assert_eq!(lp.balance(&s.beneficiary), 250_000);

        // Second release returns NothingToRelease.
        let err = client
            .try_release(&s.beneficiary, &schedule_id)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, VestingError::NothingToRelease);
    }

    #[test]
    fn test_multiple_schedules_for_same_beneficiary() {
        let s = setup();
        // Back both schedules: 1_000_000 + 250_000 must be fully funded.
        mint_to_contract(&s, 250_000);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);

        let first_id = create_schedule(&s, 0, 0, 1000);
        let second_id = client.create_vesting(
            &s.governance,
            &s.beneficiary,
            &s.lp_token,
            &s.pool,
            &250_000,
            &100,
            &200,
            &600,
        );

        assert_eq!(first_id, 0);
        assert_eq!(second_id, 1);

        let first = client.get_vesting(&s.beneficiary, &first_id);
        let second = client.get_vesting(&s.beneficiary, &second_id);
        assert_eq!(first.total, 1_000_000);
        assert_eq!(second.total, 250_000);
        assert_eq!(first.schedule_id, first_id);
        assert_eq!(second.schedule_id, second_id);

        s.env.ledger().set_sequence_number(300);
        assert_eq!(client.release(&s.beneficiary, &first_id), 300_000);
        assert_eq!(client.release(&s.beneficiary, &second_id), 62_500);
    }

    #[test]
    fn test_propose_and_accept_governance() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let new_governance = Address::generate(&s.env);

        client.propose_governance(&s.governance, &new_governance);
        assert_eq!(
            client.get_pending_governance(),
            Some(new_governance.clone())
        );

        client.accept_governance(&new_governance);
        assert_eq!(client.get_governance(), new_governance);
        assert_eq!(client.get_pending_governance(), None);
    }

    #[test]
    fn test_propose_and_accept_treasury() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let new_treasury = Address::generate(&s.env);

        client.propose_treasury(&s.governance, &new_treasury);
        assert_eq!(client.get_pending_treasury(), Some(new_treasury.clone()));

        client.accept_treasury(&new_treasury);
        assert_eq!(client.get_treasury(), new_treasury);
        assert_eq!(client.get_pending_treasury(), None);
    }

    #[test]
    fn test_propose_treasury_requires_governance() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let rando = Address::generate(&s.env);
        let new_treasury = Address::generate(&s.env);

        let err = client
            .try_propose_treasury(&rando, &new_treasury)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, VestingError::NotGovernance);
    }

    #[test]
    fn test_accept_treasury_requires_pending_nominee() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let new_treasury = Address::generate(&s.env);
        let other = Address::generate(&s.env);

        let err = client
            .try_accept_treasury(&new_treasury)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, VestingError::NoPendingTreasury);

        client.propose_treasury(&s.governance, &new_treasury);
        let err = client.try_accept_treasury(&other).unwrap_err().unwrap();
        assert_eq!(err, VestingError::NotTreasury);
    }

    #[test]
    fn test_propose_governance_requires_current_governance() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let rando = Address::generate(&s.env);
        let new_governance = Address::generate(&s.env);

        let err = client
            .try_propose_governance(&rando, &new_governance)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, VestingError::NotGovernance);
    }

    #[test]
    fn test_accept_governance_requires_pending_nominee() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let new_governance = Address::generate(&s.env);
        let other = Address::generate(&s.env);

        let err = client
            .try_accept_governance(&new_governance)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, VestingError::NoPendingGovernance);

        client.propose_governance(&s.governance, &new_governance);
        let err = client.try_accept_governance(&other).unwrap_err().unwrap();
        assert_eq!(err, VestingError::NotGovernance);
    }

    #[test]
    fn test_new_governance_controls_vesting_after_acceptance() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let new_governance = Address::generate(&s.env);

        client.propose_governance(&s.governance, &new_governance);
        client.accept_governance(&new_governance);

        let schedule_id = client.create_vesting(
            &new_governance,
            &s.beneficiary,
            &s.lp_token,
            &s.pool,
            &1_000_000,
            &0,
            &0,
            &1000,
        );

        let old_governance_err = client
            .try_revoke_vesting(&s.governance, &s.beneficiary, &schedule_id)
            .unwrap_err()
            .unwrap();
        assert_eq!(old_governance_err, VestingError::NotGovernance);

        client.revoke_vesting(&new_governance, &s.beneficiary, &schedule_id);
    }

    #[test]
    fn test_change_beneficiary() {
        let s = setup();
        let schedule_id = create_schedule(&s, 0, 100, 1000);

        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let new_beneficiary = Address::generate(&s.env);

        let new_schedule_id = client.change_beneficiary(
            &s.governance,
            &s.beneficiary,
            &schedule_id,
            &new_beneficiary,
        );

        // Old schedule should be gone
        let err = client
            .try_get_vesting(&s.beneficiary, &schedule_id)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, VestingError::VestingNotFound);

        // New schedule should exist and have same timeline
        let schedule = client.get_vesting(&new_beneficiary, &new_schedule_id);
        assert_eq!(schedule.beneficiary, new_beneficiary);
        assert_eq!(schedule.total, 1_000_000);
        assert_eq!(schedule.cliff_ledger, 100);
        assert_eq!(schedule.end_ledger, 1000);
        assert_eq!(schedule.released, 0);
    }

    // ── Issue #913: every pol_vesting event carries EVENT_SCHEMA_VERSION ──────
    //
    // These publish sites used to call `env.events().publish(...)` directly, so
    // their payloads were not version-stamped and an indexer reading
    // `(version, ...rest)` would have decoded the first real field as the
    // version number. Each test below pins the stamp for one topic.

    /// Fetch the payload of the most recent event this contract published under
    /// `topic`, decoded as a version-stamped `(u32, T)` pair.
    fn last_versioned_event<T>(s: &Setup, topic: &str) -> (u32, T)
    where
        T: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>,
    {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::IntoVal;

        let wanted: soroban_sdk::Vec<soroban_sdk::Val> =
            (Symbol::new(&s.env, topic),).into_val(&s.env);
        let evt = s
            .env
            .events()
            .all()
            .iter()
            .rfind(|e| e.0 == s.contract_id && e.1 == wanted)
            .unwrap_or_else(|| panic!("no `{topic}` event found"));
        evt.2.into_val(&s.env)
    }

    #[test]
    fn test_governance_proposed_emits_versioned_event() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let new_governance = Address::generate(&s.env);

        client.propose_governance(&s.governance, &new_governance);

        let (version, data): (u32, (Address, Address)) =
            last_versioned_event(&s, "governance_proposed");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (s.governance.clone(), new_governance));
    }

    #[test]
    fn test_governance_transferred_emits_versioned_event() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let new_governance = Address::generate(&s.env);

        client.propose_governance(&s.governance, &new_governance);
        client.accept_governance(&new_governance);

        let (version, data): (u32, (Address, Address)) =
            last_versioned_event(&s, "governance_transferred");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (s.governance.clone(), new_governance));
    }

    #[test]
    fn test_treasury_proposed_emits_versioned_event() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let new_treasury = Address::generate(&s.env);

        client.propose_treasury(&s.governance, &new_treasury);

        let (version, data): (u32, (Address, Address)) =
            last_versioned_event(&s, "treasury_proposed");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (s.governance.clone(), new_treasury));
    }

    #[test]
    fn test_treasury_transferred_emits_versioned_event() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let new_treasury = Address::generate(&s.env);

        client.propose_treasury(&s.governance, &new_treasury);
        client.accept_treasury(&new_treasury);

        let (version, data): (u32, (Address, Address)) =
            last_versioned_event(&s, "treasury_transferred");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (s.treasury.clone(), new_treasury));
    }

    #[test]
    fn test_vesting_created_emits_versioned_event() {
        let s = setup();
        let schedule_id = create_schedule(&s, 0, 0, 1000);

        let (version, data): (u32, (Address, u32, i128, u32, u32, u32)) =
            last_versioned_event(&s, "vesting_created");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(
            data,
            (s.beneficiary.clone(), schedule_id, 1_000_000, 0, 0, 1000)
        );
    }

    #[test]
    fn test_released_emits_versioned_event() {
        let s = setup();
        let schedule_id = create_schedule(&s, 0, 0, 1000);
        s.env.ledger().set_sequence_number(500);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        client.release(&s.beneficiary, &schedule_id);

        let (version, data): (u32, (Address, u32, i128)) = last_versioned_event(&s, "released");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (s.beneficiary.clone(), schedule_id, 500_000));
    }

    #[test]
    fn test_beneficiary_changed_emits_versioned_event() {
        let s = setup();
        let schedule_id = create_schedule(&s, 0, 100, 1000);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let new_beneficiary = Address::generate(&s.env);

        let new_schedule_id = client.change_beneficiary(
            &s.governance,
            &s.beneficiary,
            &schedule_id,
            &new_beneficiary,
        );

        let (version, data): (u32, (Address, u32, Address, u32)) =
            last_versioned_event(&s, "beneficiary_changed");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(
            data,
            (
                s.beneficiary.clone(),
                schedule_id,
                new_beneficiary,
                new_schedule_id
            )
        );
    }

    #[test]
    fn test_vesting_revoked_emits_versioned_event() {
        let s = setup();
        let schedule_id = create_schedule(&s, 0, 0, 1000);
        s.env.ledger().set_sequence_number(250);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        client.revoke_vesting(&s.governance, &s.beneficiary, &schedule_id);

        let (version, data): (u32, (Address, u32, i128, i128)) =
            last_versioned_event(&s, "vesting_revoked");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (s.beneficiary.clone(), schedule_id, 250_000, 750_000));
    }

    // ── Instance TTL regression (issue #908) ────────────────────────────────
    // Vesting schedules are long-lived, so the instance entry (governance,
    // treasury and pending nominees) is exactly the entry that lapses from
    // disuse between claims. If it is archived a returning beneficiary finds
    // the contract unreachable. These tests drive the ledger past the instance
    // TTL, then confirm entrypoints still respond and re-extend it.

    fn instance_ttl(s: &Setup) -> u32 {
        s.env
            .as_contract(&s.contract_id, || s.env.storage().instance().get_ttl())
    }

    fn lower_instance_ttl_below_min(s: &Setup) {
        s.env
            .ledger()
            .with_mut(|l| l.sequence_number += BUMP_TO - MIN_TTL + 1);
        let ttl = instance_ttl(s);
        assert!(
            ttl < MIN_TTL,
            "test setup should lower instance TTL below MIN_TTL, got {ttl}"
        );
    }

    fn assert_instance_ttl_bumped(s: &Setup) {
        let ttl = instance_ttl(s);
        assert!(
            ttl >= BUMP_TO - 1,
            "instance TTL {ttl} should be bumped toward BUMP_TO"
        );
    }

    #[test]
    fn test_initialize_extends_instance_ttl() {
        let s = setup();
        assert_instance_ttl_bumped(&s);
    }

    #[test]
    fn test_read_entrypoint_restores_lapsed_instance_ttl() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);

        lower_instance_ttl_below_min(&s);

        // A returning beneficiary or integrator often reads governance/treasury
        // first; that pure read must still respond and restore the instance TTL.
        assert_eq!(client.get_governance(), s.governance);
        assert_instance_ttl_bumped(&s);
    }

    #[test]
    fn test_write_entrypoint_restores_lapsed_instance_ttl() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let new_treasury = Address::generate(&s.env);

        lower_instance_ttl_below_min(&s);

        // `propose_treasury` writes only instance state, isolating the write
        // path's TTL bump from the persistent schedule entries.
        client.propose_treasury(&s.governance, &new_treasury);

        assert_eq!(client.get_pending_treasury(), Some(new_treasury));
        assert_instance_ttl_bumped(&s);
    }

    // ── Funding / commitment accounting (issue #1044) ───────────────────────
    //
    // On main, `create_vesting` never compared `total` against the contract's
    // balance, and every schedule on one LP token drew from the same shared
    // balance. Two schedules could each look affordable while together
    // over-committing the token, and whoever released first drained tokens the
    // other schedule had already promised. These tests pin the commitment book.

    /// Call `create_vesting` for an arbitrary beneficiary/token/amount.
    #[allow(clippy::too_many_arguments)]
    fn create(
        s: &Setup,
        beneficiary: &Address,
        lp_token: &Address,
        total: i128,
        start: u32,
        cliff: u32,
        end: u32,
    ) -> Result<u32, VestingError> {
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        match client.try_create_vesting(
            &s.governance,
            beneficiary,
            lp_token,
            &s.pool,
            &total,
            &start,
            &cliff,
            &end,
        ) {
            Ok(Ok(id)) => Ok(id),
            Err(Ok(err)) => Err(err),
            _ => panic!("unexpected create_vesting invocation failure"),
        }
    }

    #[test]
    fn test_create_vesting_rejects_insufficient_funding() {
        let s = setup_funded(500_000);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);

        let err = create(&s, &s.beneficiary, &s.lp_token, 1_000_000, 0, 0, 1000).unwrap_err();
        assert_eq!(err, VestingError::InsufficientFunding);

        // A rejected create must leave no trace.
        assert_eq!(client.committed(&s.lp_token), 0);
        assert_eq!(client.schedule_count(&s.beneficiary), 0);
        assert_eq!(
            client
                .try_get_vesting(&s.beneficiary, &0)
                .unwrap_err()
                .unwrap(),
            VestingError::VestingNotFound
        );
    }

    #[test]
    fn test_create_vesting_allows_exactly_the_uncommitted_balance() {
        let s = setup_funded(1_000_000);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);

        let id = create_schedule(&s, 0, 0, 1000);
        assert_eq!(id, 0);
        assert_eq!(client.committed(&s.lp_token), 1_000_000);

        // Not a single token remains, so even a 1-token schedule is refused.
        let err = create(&s, &s.beneficiary, &s.lp_token, 1, 0, 0, 1000).unwrap_err();
        assert_eq!(err, VestingError::InsufficientFunding);
    }

    #[test]
    fn test_schedules_cannot_share_the_same_uncommitted_tokens() {
        let s = setup_funded(1_000_000);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);

        // 700_000 is affordable on its own...
        create(&s, &s.beneficiary, &s.lp_token, 700_000, 0, 0, 1000).unwrap();
        assert_eq!(client.committed(&s.lp_token), 700_000);

        // ...but a second 700_000 would over-commit the shared 1_000_000
        // balance. On main this call succeeded, and both schedules then raced to
        // drain the same tokens on release.
        let err = create(&s, &s.beneficiary, &s.lp_token, 700_000, 0, 0, 1000).unwrap_err();
        assert_eq!(err, VestingError::InsufficientFunding);
        assert_eq!(client.schedule_count(&s.beneficiary), 1);
    }

    #[test]
    fn test_two_schedules_on_one_token_both_release_in_full() {
        let s = setup_funded(1_000_000);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let b1 = Address::generate(&s.env);
        let b2 = Address::generate(&s.env);

        let id1 = create(&s, &b1, &s.lp_token, 500_000, 0, 0, 1000).unwrap();
        let id2 = create(&s, &b2, &s.lp_token, 500_000, 0, 0, 1000).unwrap();
        assert_eq!(client.committed(&s.lp_token), 1_000_000);

        s.env.ledger().set_sequence_number(1000);
        assert_eq!(client.release(&b1, &id1), 500_000);
        assert_eq!(client.release(&b2, &id2), 500_000);
        assert_eq!(client.committed(&s.lp_token), 0);

        let lp = TokenClient::new(&s.env, &s.lp_token);
        assert_eq!(lp.balance(&b1), 500_000);
        assert_eq!(lp.balance(&b2), 500_000);
        assert_eq!(lp.balance(&s.contract_id), 0);
    }

    #[test]
    fn test_release_decreases_committed() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let id = create_schedule(&s, 0, 0, 1000);

        s.env.ledger().set_sequence_number(500);
        client.release(&s.beneficiary, &id);
        assert_eq!(client.committed(&s.lp_token), 500_000);

        s.env.ledger().set_sequence_number(1000);
        client.release(&s.beneficiary, &id);
        assert_eq!(client.committed(&s.lp_token), 0);
    }

    #[test]
    #[should_panic(expected = "pol_vesting: committed underflow")]
    fn test_release_traps_instead_of_silently_corrupting_committed() {
        // `committed` should never fall below what `release` is about to
        // subtract from it — if it ever does (a bug elsewhere in the
        // commitment bookkeeping), the subtraction must trap rather than
        // silently write a negative `committed`, which would then inflate
        // `create_vesting`'s `balance - committed` affordability check.
        let s = setup();
        let id = create_schedule(&s, 0, 0, 1000);

        s.env.as_contract(&s.contract_id, || {
            s.env
                .storage()
                .persistent()
                .set(&DataKey::Committed(s.lp_token.clone()), &0i128);
        });

        s.env.ledger().set_sequence_number(500);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        client.release(&s.beneficiary, &id);
    }

    #[test]
    #[should_panic(expected = "pol_vesting: committed underflow")]
    fn test_revoke_traps_instead_of_silently_corrupting_committed() {
        let s = setup();
        let id = create_schedule(&s, 0, 0, 1000);

        s.env.as_contract(&s.contract_id, || {
            s.env
                .storage()
                .persistent()
                .set(&DataKey::Committed(s.lp_token.clone()), &0i128);
        });

        s.env.ledger().set_sequence_number(250);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        client.revoke_vesting(&s.governance, &s.beneficiary, &id);
    }

    #[test]
    fn test_revoke_decreases_committed() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let id = create_schedule(&s, 0, 0, 1000);
        assert_eq!(client.committed(&s.lp_token), 1_000_000);

        s.env.ledger().set_sequence_number(250);
        client.revoke_vesting(&s.governance, &s.beneficiary, &id);

        // Treasury takes 750_000, so committed drops by 750_000 to 250_000
        assert_eq!(client.committed(&s.lp_token), 250_000);
        assert_eq!(client.schedule_count(&s.beneficiary), 0);

        // Claiming the remainder via release reduces committed to 0
        assert_eq!(client.release(&s.beneficiary, &id), 250_000);
        assert_eq!(client.committed(&s.lp_token), 0);
    }

    #[test]
    fn test_change_beneficiary_moves_the_live_list_entry() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let id = create_schedule(&s, 0, 0, 1000);
        let new_beneficiary = Address::generate(&s.env);
        let committed_before = client.committed(&s.lp_token);

        let new_id =
            client.change_beneficiary(&s.governance, &s.beneficiary, &id, &new_beneficiary);

        // The move conserves the commitment and shifts the schedule to the new
        // beneficiary's list without leaving an entry behind.
        assert_eq!(client.committed(&s.lp_token), committed_before);
        assert_eq!(client.schedule_count(&s.beneficiary), 0);
        assert_eq!(client.schedule_count(&new_beneficiary), 1);
        let listed = client.list_schedules(&new_beneficiary, &0, &10);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed.get(0).unwrap().schedule_id, new_id);
    }

    #[test]
    fn test_change_beneficiary_to_same_beneficiary_does_not_duplicate() {
        let s = setup_funded(0);
        mint_to_contract(&s, i128::from(MAX_SCHEDULES_PER_BENEFICIARY));
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let id = create(&s, &s.beneficiary, &s.lp_token, 1, 0, 0, 1000).unwrap();
        for _ in 1..MAX_SCHEDULES_PER_BENEFICIARY {
            create(&s, &s.beneficiary, &s.lp_token, 1, 0, 0, 1000).unwrap();
        }
        assert_eq!(
            client.schedule_count(&s.beneficiary),
            MAX_SCHEDULES_PER_BENEFICIARY
        );

        // Moving within the same beneficiary preserves the count, even at cap.
        let new_id = client.change_beneficiary(&s.governance, &s.beneficiary, &id, &s.beneficiary);

        assert_eq!(
            client.schedule_count(&s.beneficiary),
            MAX_SCHEDULES_PER_BENEFICIARY
        );
        // list_schedules is capped at MAX_PAGE (50) per call; fetch two pages to
        // cover all 100 schedules.
        let mut all_ids: soroban_sdk::Vec<u32> = soroban_sdk::Vec::new(&s.env);
        let page0 = client.list_schedules(&s.beneficiary, &0, &MAX_SCHEDULES_PER_BENEFICIARY);
        for sched in page0.iter() {
            all_ids.push_back(sched.schedule_id);
        }
        let page1 =
            client.list_schedules(&s.beneficiary, &MAX_PAGE, &MAX_SCHEDULES_PER_BENEFICIARY);
        for sched in page1.iter() {
            all_ids.push_back(sched.schedule_id);
        }
        assert_eq!(all_ids.len(), MAX_SCHEDULES_PER_BENEFICIARY);
        assert!(all_ids.iter().any(|id| id == new_id));
        // The stale id is gone.
        assert_eq!(
            client
                .try_get_vesting(&s.beneficiary, &id)
                .unwrap_err()
                .unwrap(),
            VestingError::VestingNotFound
        );
    }

    #[test]
    fn test_committed_is_tracked_per_lp_token() {
        let s = setup_funded(1_000_000);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let other = s
            .env
            .register_stellar_asset_contract_v2(s.governance.clone())
            .address();
        StellarAssetClient::new(&s.env, &other).mint(&s.contract_id, &300_000);

        create(&s, &s.beneficiary, &s.lp_token, 1_000_000, 0, 0, 1000).unwrap();
        create(&s, &s.beneficiary, &other, 300_000, 0, 0, 1000).unwrap();

        assert_eq!(client.committed(&s.lp_token), 1_000_000);
        assert_eq!(client.committed(&other), 300_000);
        assert_eq!(client.committed(&Address::generate(&s.env)), 0);
    }

    #[test]
    fn test_committed_is_renewed_when_a_lapsed_entry_is_read() {
        // `read_committed` renews the `Committed` entry on every read, because a
        // schedule can sit fully committed for months with no create, release or
        // revoke to touch it, and reads are the only thing a client does with
        // `committed`. A lapsed-but-not-yet-archived entry must not be allowed to
        // age out from under the balance it describes.
        //
        // The renewal itself is not assertable from a test: `Instance` exposes
        // `get_ttl` but `Persistent` does not, and EnvTest treats an archived
        // entry as gone rather than restorable, so driving the ledger far enough
        // to age the entry out would assert the test harness's archival model
        // instead of the contract's behaviour.
        let s = setup_funded(1_000_000);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        create(&s, &s.beneficiary, &s.lp_token, 1_000_000, 0, 0, 1000).unwrap();

        // Reading a committed token repeatedly must stay correct and must leave
        // the balance fully committed (the entry is still there to renew).
        for _ in 0..3 {
            assert_eq!(client.committed(&s.lp_token), 1_000_000);
        }

        let other = Address::generate(&s.env);
        let err = create(&s, &other, &s.lp_token, 1, 0, 0, 1000).unwrap_err();
        assert_eq!(err, VestingError::InsufficientFunding);
    }

    // ── Enumeration and preview views (issue #1044) ─────────────────────────

    #[test]
    fn test_releasable_amount_previews_release() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let id = create_schedule(&s, 100, 200, 400);

        // Before the cliff nothing vests.
        s.env.ledger().set_sequence_number(150);
        assert_eq!(client.vested_amount(&s.beneficiary, &id), 0);
        assert_eq!(client.releasable_amount(&s.beneficiary, &id), 0);

        // Midway: 50% vested, all of it releasable, and the release pays the
        // previewed amount exactly.
        s.env.ledger().set_sequence_number(300);
        assert_eq!(client.vested_amount(&s.beneficiary, &id), 500_000);
        let preview = client.releasable_amount(&s.beneficiary, &id);
        assert_eq!(preview, 500_000);
        assert_eq!(client.release(&s.beneficiary, &id), preview);

        // After a release the preview drops to zero while accrual is unchanged.
        assert_eq!(client.vested_amount(&s.beneficiary, &id), 500_000);
        assert_eq!(client.releasable_amount(&s.beneficiary, &id), 0);

        // At the end the remainder becomes releasable.
        s.env.ledger().set_sequence_number(400);
        assert_eq!(client.releasable_amount(&s.beneficiary, &id), 500_000);
    }

    #[test]
    fn test_amount_views_error_on_missing_schedule() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);

        assert_eq!(
            client
                .try_releasable_amount(&s.beneficiary, &0)
                .unwrap_err()
                .unwrap(),
            VestingError::VestingNotFound
        );
        assert_eq!(
            client
                .try_vested_amount(&s.beneficiary, &0)
                .unwrap_err()
                .unwrap(),
            VestingError::VestingNotFound
        );
    }

    #[test]
    fn test_list_schedules_paginates_without_gaps() {
        let s = setup_funded(0);
        mint_to_contract(&s, 55);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        for _ in 0..55u32 {
            create(&s, &s.beneficiary, &s.lp_token, 1, 0, 0, 1000).unwrap();
        }
        assert_eq!(client.schedule_count(&s.beneficiary), 55);

        let page = client.list_schedules(&s.beneficiary, &0, &4);
        assert_eq!(page.len(), 4);
        assert_eq!(page.get(0).unwrap().schedule_id, 0);
        assert_eq!(page.get(3).unwrap().schedule_id, 3);

        let tail = client.list_schedules(&s.beneficiary, &53, &4);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail.get(1).unwrap().schedule_id, 54);

        // Offset past the end and a zero limit both yield an empty page.
        assert_eq!(client.list_schedules(&s.beneficiary, &55, &4).len(), 0);
        assert_eq!(client.list_schedules(&s.beneficiary, &0, &0).len(), 0);

        // An oversized page is clamped to MAX_PAGE.
        assert_eq!(
            client.list_schedules(&s.beneficiary, &0, &u32::MAX).len(),
            MAX_PAGE
        );
    }

    #[test]
    fn test_schedule_cap_is_enforced() {
        let s = setup_funded(0);
        mint_to_contract(&s, i128::from(MAX_SCHEDULES_PER_BENEFICIARY) + 1);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);

        for _ in 0..MAX_SCHEDULES_PER_BENEFICIARY {
            create(&s, &s.beneficiary, &s.lp_token, 1, 0, 0, 1000).unwrap();
        }
        assert_eq!(
            client.schedule_count(&s.beneficiary),
            MAX_SCHEDULES_PER_BENEFICIARY
        );
        // A 101st schedule is refused even though the tokens are available.
        let err = create(&s, &s.beneficiary, &s.lp_token, 1, 0, 0, 1000).unwrap_err();
        assert_eq!(err, VestingError::TooManySchedules);
    }

    #[test]
    fn test_revoked_id_leaves_no_gap_in_enumeration() {
        let s = setup_funded(0);
        mint_to_contract(&s, 3);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);

        let a = create(&s, &s.beneficiary, &s.lp_token, 1, 0, 0, 1000).unwrap();
        create(&s, &s.beneficiary, &s.lp_token, 1, 0, 0, 1000).unwrap();
        create(&s, &s.beneficiary, &s.lp_token, 1, 0, 0, 1000).unwrap();

        // Revoke id 0; the surviving ids 1 and 2 enumerate densely.
        client.revoke_vesting(&s.governance, &s.beneficiary, &a);
        assert_eq!(client.schedule_count(&s.beneficiary), 2);
        let listed = client.list_schedules(&s.beneficiary, &0, &10);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed.get(0).unwrap().schedule_id, 1);
        assert_eq!(listed.get(1).unwrap().schedule_id, 2);
    }

    // ── Property: the contract never owes more than it holds ─────────────────
    //
    // Drives a deterministic sequence of creates, releases, revocations and
    // beneficiary moves across several tokens and beneficiaries; after every
    // step, each token's contract balance must cover its commitment. This is
    // the cross-drain invariant: no schedule can ever promise tokens that a
    // different schedule has already promised.

    #[test]
    fn test_property_balance_never_falls_below_committed() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();

        let governance = Address::generate(&env);
        let treasury = Address::generate(&env);
        let pool = Address::generate(&env);
        let contract_id = env.register_contract(None, PolVestingContract);
        let client = PolVestingContractClient::new(&env, &contract_id);
        client.initialize(&governance, &treasury);

        let tokens: [Address; 3] = [
            env.register_stellar_asset_contract_v2(governance.clone())
                .address(),
            env.register_stellar_asset_contract_v2(governance.clone())
                .address(),
            env.register_stellar_asset_contract_v2(governance.clone())
                .address(),
        ];
        for t in tokens.iter() {
            StellarAssetClient::new(&env, t).mint(&contract_id, &1_000_000);
        }

        let beneficiaries: [Address; 4] = [
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
            Address::generate(&env),
        ];

        // Deterministic xorshift64 PRNG so the run is reproducible.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for _ in 0..200 {
            let op = next() % 5;
            let b = beneficiaries[(next() % 4) as usize].clone();

            match op {
                0 => {
                    let ti = (next() % 3) as usize;
                    let total = i128::from(next() % 50_000 + 1);
                    let start = (next() % 100) as u32;
                    let cliff = start + (next() % 100) as u32;
                    let end = cliff + 1 + (next() % 500) as u32;
                    let _ = client.try_create_vesting(
                        &governance,
                        &b,
                        &tokens[ti],
                        &pool,
                        &total,
                        &start,
                        &cliff,
                        &end,
                    );
                }
                1 | 2 => {
                    let count = client.schedule_count(&b);
                    if count > 0 {
                        let sched = client
                            .list_schedules(&b, &((next() % u64::from(count)) as u32), &1)
                            .get(0)
                            .unwrap();
                        let _ = client.try_release(&b, &sched.schedule_id);
                    }
                }
                3 => {
                    let count = client.schedule_count(&b);
                    if count > 0 {
                        let sched = client
                            .list_schedules(&b, &((next() % u64::from(count)) as u32), &1)
                            .get(0)
                            .unwrap();
                        let _ = client.try_revoke_vesting(&governance, &b, &sched.schedule_id);
                    }
                }
                _ => {
                    let count = client.schedule_count(&b);
                    if count > 0 {
                        let sched = client
                            .list_schedules(&b, &((next() % u64::from(count)) as u32), &1)
                            .get(0)
                            .unwrap();
                        let nb = beneficiaries[(next() % 4) as usize].clone();
                        let _ =
                            client.try_change_beneficiary(&governance, &b, &sched.schedule_id, &nb);
                    }
                }
            }

            // Advance the ledger so schedules accrue at varying rates.
            env.ledger()
                .with_mut(|l| l.sequence_number += 1 + (next() % 20) as u32);

            for t in tokens.iter() {
                let balance = TokenClient::new(&env, t).balance(&contract_id);
                let committed = client.committed(t);
                assert!(
                    balance >= committed,
                    "token balance {balance} < committed {committed}"
                );
            }
        }
    }

    // ── Tests for Issue #1073: revoke_vesting never transfers to beneficiary ──

    #[test]
    fn test_revoke_never_transfers_to_beneficiary() {
        let s = setup();
        let schedule_id = create_schedule(&s, 0, 0, 1000);

        s.env.ledger().set_sequence_number(500);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let lp = TokenClient::new(&s.env, &s.lp_token);

        let bal_before = lp.balance(&s.beneficiary);
        client.revoke_vesting(&s.governance, &s.beneficiary, &schedule_id);
        let bal_after = lp.balance(&s.beneficiary);

        // revoke_vesting NEVER transfers to beneficiary
        assert_eq!(bal_before, 0);
        assert_eq!(bal_after, 0);
        assert_eq!(lp.balance(&s.treasury), 500_000);
    }

    #[test]
    fn test_revoke_simulation_apply_regression_identical_recipients() {
        // Build the same state twice (create, release at ledger L).
        // Call revoke_vesting at ledger L in env1 and at L+1 in env2.
        // Assert the set of transfer recipients is identical in both (only treasury).
        let s1 = setup();
        let s2 = setup();

        let id1 = create_schedule(&s1, 0, 0, 1000);
        let id2 = create_schedule(&s2, 0, 0, 1000);

        let l = 200;
        s1.env.ledger().set_sequence_number(l);
        s2.env.ledger().set_sequence_number(l);

        let client1 = PolVestingContractClient::new(&s1.env, &s1.contract_id);
        let client2 = PolVestingContractClient::new(&s2.env, &s2.contract_id);

        let lp1 = TokenClient::new(&s1.env, &s1.lp_token);
        let lp2 = TokenClient::new(&s2.env, &s2.lp_token);

        // Both release at ledger L
        assert_eq!(client1.release(&s1.beneficiary, &id1), 200_000);
        assert_eq!(client2.release(&s2.beneficiary, &id2), 200_000);

        // Env1 calls revoke at ledger L
        let b1_before = lp1.balance(&s1.beneficiary);
        let t1_before = lp1.balance(&s1.treasury);
        client1.revoke_vesting(&s1.governance, &s1.beneficiary, &id1);
        let b1_diff = lp1.balance(&s1.beneficiary) - b1_before;
        let t1_diff = lp1.balance(&s1.treasury) - t1_before;

        // Env2 advances to ledger L+1 before calling revoke
        s2.env.ledger().set_sequence_number(l + 1);
        let b2_before = lp2.balance(&s2.beneficiary);
        let t2_before = lp2.balance(&s2.treasury);
        client2.revoke_vesting(&s2.governance, &s2.beneficiary, &id2);
        let b2_diff = lp2.balance(&s2.beneficiary) - b2_before;
        let t2_diff = lp2.balance(&s2.treasury) - t2_before;

        // In both environments, beneficiary received exactly 0 from revoke_vesting
        assert_eq!(b1_diff, 0);
        assert_eq!(b2_diff, 0);

        // In both environments, the only transfer recipient is the treasury
        assert!(t1_diff > 0);
        assert!(t2_diff > 0);
        assert_eq!(t1_diff, 800_000);
        assert_eq!(t2_diff, 799_000);
    }

    #[test]
    fn test_revoke_beneficiary_claims_remainder_via_release() {
        let s = setup();
        let id = create_schedule(&s, 0, 0, 1000);

        s.env.ledger().set_sequence_number(200);
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        assert_eq!(client.release(&s.beneficiary, &id), 200_000);

        s.env.ledger().set_sequence_number(600);
        client.revoke_vesting(&s.governance, &s.beneficiary, &id);

        // Claimable is frozen at 600_000 - 200_000 = 400_000
        assert_eq!(client.releasable_amount(&s.beneficiary, &id), 400_000);

        // Advance far past end_ledger; releasable_amount remains frozen
        s.env.ledger().set_sequence_number(5000);
        assert_eq!(client.releasable_amount(&s.beneficiary, &id), 400_000);

        // Claim frozen remainder
        let claimed = client.release(&s.beneficiary, &id);
        assert_eq!(claimed, 400_000);

        let lp = TokenClient::new(&s.env, &s.lp_token);
        assert_eq!(lp.balance(&s.beneficiary), 600_000);

        // Second release returns NothingToRelease even after end_ledger
        let err = client
            .try_release(&s.beneficiary, &id)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, VestingError::NothingToRelease);
    }

    #[test]
    fn test_conservation_revoke_before_cliff() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let id = create_schedule(&s, 100, 300, 1000);

        s.env.ledger().set_sequence_number(200);
        client.revoke_vesting(&s.governance, &s.beneficiary, &id);

        let lp = TokenClient::new(&s.env, &s.lp_token);
        let released = 0i128;
        let claimable = client.releasable_amount(&s.beneficiary, &id);
        let to_treasury = lp.balance(&s.treasury);

        assert_eq!(claimable, 0);
        assert_eq!(to_treasury, 1_000_000);
        assert_eq!(released + claimable + to_treasury, 1_000_000);

        let err = client
            .try_release(&s.beneficiary, &id)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, VestingError::NothingToRelease);
    }

    #[test]
    fn test_conservation_revoke_mid_vesting() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let id = create_schedule(&s, 0, 200, 1000);

        // Release at ledger 400: elapsed = 200, duration = 800 -> 250_000
        s.env.ledger().set_sequence_number(400);
        let released = client.release(&s.beneficiary, &id);
        assert_eq!(released, 250_000);

        // Revoke at ledger 600: elapsed = 400, duration = 800 -> 500_000 vested
        s.env.ledger().set_sequence_number(600);
        client.revoke_vesting(&s.governance, &s.beneficiary, &id);

        let lp = TokenClient::new(&s.env, &s.lp_token);
        let claimable = client.releasable_amount(&s.beneficiary, &id);
        let to_treasury = lp.balance(&s.treasury);

        assert_eq!(claimable, 250_000);
        assert_eq!(to_treasury, 500_000);
        assert_eq!(released + claimable + to_treasury, 1_000_000);

        // Claim remainder
        let remainder = client.release(&s.beneficiary, &id);
        assert_eq!(remainder, 250_000);
        assert_eq!(lp.balance(&s.beneficiary), 500_000);

        let err = client
            .try_release(&s.beneficiary, &id)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, VestingError::NothingToRelease);
    }

    #[test]
    fn test_conservation_revoke_after_end() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let id = create_schedule(&s, 0, 0, 1000);

        s.env.ledger().set_sequence_number(400);
        let released = client.release(&s.beneficiary, &id);
        assert_eq!(released, 400_000);

        s.env.ledger().set_sequence_number(1200);
        client.revoke_vesting(&s.governance, &s.beneficiary, &id);

        let lp = TokenClient::new(&s.env, &s.lp_token);
        let claimable = client.releasable_amount(&s.beneficiary, &id);
        let to_treasury = lp.balance(&s.treasury);

        assert_eq!(claimable, 600_000);
        assert_eq!(to_treasury, 0);
        assert_eq!(released + claimable + to_treasury, 1_000_000);

        let remainder = client.release(&s.beneficiary, &id);
        assert_eq!(remainder, 600_000);
        assert_eq!(lp.balance(&s.beneficiary), 1_000_000);

        let err = client
            .try_release(&s.beneficiary, &id)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, VestingError::NothingToRelease);
    }

    #[test]
    fn test_get_vesting_on_revoked_schedule_reports_revoked_and_frozen_total() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let id = create_schedule(&s, 0, 0, 1000);

        s.env.ledger().set_sequence_number(350);
        client.revoke_vesting(&s.governance, &s.beneficiary, &id);

        let sched = client.get_vesting(&s.beneficiary, &id);
        assert!(sched.revoked);
        assert_eq!(sched.total, 350_000);
        assert_eq!(sched.released, 0);
        assert_eq!(client.vested_amount(&s.beneficiary, &id), 350_000);

        s.env.ledger().set_sequence_number(9999);
        assert_eq!(client.vested_amount(&s.beneficiary, &id), 350_000);
        assert_eq!(client.releasable_amount(&s.beneficiary, &id), 350_000);
    }

    #[test]
    fn test_cannot_revoke_already_revoked_schedule() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let id = create_schedule(&s, 0, 0, 1000);

        client.revoke_vesting(&s.governance, &s.beneficiary, &id);
        let err = client
            .try_revoke_vesting(&s.governance, &s.beneficiary, &id)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, VestingError::InvalidSchedule);
    }

    #[test]
    fn test_cannot_change_beneficiary_of_revoked_schedule() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let id = create_schedule(&s, 0, 0, 1000);

        client.revoke_vesting(&s.governance, &s.beneficiary, &id);
        let new_beneficiary = Address::generate(&s.env);
        let err = client
            .try_change_beneficiary(&s.governance, &s.beneficiary, &id, &new_beneficiary)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, VestingError::InvalidSchedule);
    }

    #[test]
    fn test_revoke_committed_accounting_only_deducts_treasury() {
        let s = setup();
        let client = PolVestingContractClient::new(&s.env, &s.contract_id);
        let id = create_schedule(&s, 0, 0, 1000);
        assert_eq!(client.committed(&s.lp_token), 1_000_000);

        s.env.ledger().set_sequence_number(400);
        client.revoke_vesting(&s.governance, &s.beneficiary, &id);

        // Treasury received 600_000, committed drops from 1_000_000 to 400_000
        assert_eq!(client.committed(&s.lp_token), 400_000);

        // Beneficiary claims the remaining 400_000, committed drops to 0
        client.release(&s.beneficiary, &id);
        assert_eq!(client.committed(&s.lp_token), 0);
    }
}
