//! Protocol-Owned Liquidity (POL) Vesting Contract
//!
//! Governance can create time-based vesting schedules for LP tokens so that
//! protocol-owned liquidity cannot be withdrawn in a single governance vote.
//! Tokens vest linearly between `cliff_ledger` and `end_ledger`.

#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Symbol,
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
}

// ── Constants ─────────────────────────────────────────────────────────────────

const MIN_TTL: u32 = 241_920; // ~14 days (at 5s per ledger)
const BUMP_TO: u32 = 3_110_400; // ~180 days (at 5s per ledger)

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

        let next_id_key = DataKey::NextScheduleId(beneficiary.clone());
        let schedule_id: u32 = env.storage().persistent().get(&next_id_key).unwrap_or(0);
        let key = DataKey::Vesting(beneficiary.clone(), schedule_id);
        if env.storage().persistent().has(&key) {
            return Err(VestingError::VestingAlreadyExists);
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
        let releasable = Self::vested_amount(&schedule, current_ledger) - schedule.released;
        if releasable <= 0 {
            return Err(VestingError::NothingToRelease);
        }

        schedule.released += releasable;
        env.storage().persistent().set(&key, &schedule);

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

        env.storage().persistent().remove(&old_key);

        let next_id_key = DataKey::NextScheduleId(new_beneficiary.clone());
        let new_schedule_id: u32 = env.storage().persistent().get(&next_id_key).unwrap_or(0);
        let new_key = DataKey::Vesting(new_beneficiary.clone(), new_schedule_id);

        if env.storage().persistent().has(&new_key) {
            return Err(VestingError::VestingAlreadyExists);
        }

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

    /// Governance cancels a vesting schedule. Any unreleased tokens are
    /// returned to the treasury; already-released tokens are unaffected.
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
        let schedule: PolVesting = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(VestingError::VestingNotFound)?;

        env.storage()
            .persistent()
            .extend_ttl(&key, MIN_TTL, BUMP_TO);

        let current_ledger = env.ledger().sequence();
        let vested = Self::vested_amount(&schedule, current_ledger);
        // Tokens already vested but not yet released go to beneficiary first.
        let to_beneficiary = vested - schedule.released;
        let to_treasury = schedule.total - vested;

        let lp = token::Client::new(&env, &schedule.lp_token);
        let contract_addr = env.current_contract_address();

        if to_beneficiary > 0 {
            lp.transfer(&contract_addr, &beneficiary, &to_beneficiary);
        }
        if to_treasury > 0 {
            let treasury: Address = env.storage().instance().get(&DataKey::Treasury).unwrap();
            lp.transfer(&contract_addr, &treasury, &to_treasury);
        }

        env.storage().persistent().remove(&key);

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

    /// Linear vesting between `cliff_ledger` and `end_ledger`; 0 before the
    /// cliff; `total` at or after `end_ledger`.
    ///
    /// Accrual starts at the cliff, not at `start_ledger`. Measuring elapsed
    /// time from `start_ledger` (while gating release on the cliff) would make
    /// the vested amount jump discontinuously to `total * (cliff - start) /
    /// (end - start)` the instant the cliff is reached — a lump-sum unlock of
    /// protocol-owned liquidity that contradicts the documented schedule and
    /// the "cannot be withdrawn in a single step" guarantee.
    fn vested_amount(schedule: &PolVesting, current_ledger: u32) -> i128 {
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

        // Mint 1_000_000 LP tokens to the vesting contract.
        StellarAssetClient::new(&env, &lp_token).mint(&contract_id, &1_000_000);

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
        // 25% went to beneficiary, 75% to treasury
        assert_eq!(lp.balance(&s.beneficiary), 250_000);
        assert_eq!(lp.balance(&s.treasury), 750_000);

        // Schedule is gone.
        let err = client
            .try_get_vesting(&s.beneficiary, &schedule_id)
            .unwrap_err()
            .unwrap();
        assert_eq!(err, VestingError::VestingNotFound);
    }

    #[test]
    fn test_multiple_schedules_for_same_beneficiary() {
        let s = setup();
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

        let (version, data): (u32, (Address, u32, i128)) =
            last_versioned_event(&s, "released");
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
}
