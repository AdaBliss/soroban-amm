//! Tests for the #1136 hardening: LP token and pool validation at creation,
//! TTL for schedules that outlive the default bump, the `initialized` event,
//! the governance-gated `upgrade`, and a rejected `change_beneficiary` leaving
//! state untouched.

extern crate std;

use super::*;
use soroban_sdk::{
    contract, contractimpl, contracttype,
    testutils::{Address as _, Events, Ledger},
    token::StellarAssetClient,
    Address, BytesN, Env, Symbol, TryFromVal,
};

/// Ledgers in one day at 5 seconds per ledger.
const DAY_LEDGERS: u32 = 17_280;
const TOTAL: i128 = 1_000_000;

/// An LP token whose entries stay alive on every call, so the long-schedule
/// tests measure the vesting contract's TTL handling and not an unrelated
/// token's archival. A live LP token is a busy contract in its own right.
#[contract]
pub struct KeepAliveLpToken;

#[contracttype]
enum LpKey {
    Admin,
    Balance(Address),
}

#[contractimpl]
impl KeepAliveLpToken {
    pub fn initialize(env: Env, admin: Address) {
        Self::keep_alive(&env);
        env.storage().instance().set(&LpKey::Admin, &admin);
    }

    pub fn admin(env: Env) -> Address {
        Self::keep_alive(&env);
        env.storage().instance().get(&LpKey::Admin).unwrap()
    }

    pub fn mint(env: Env, to: Address, amount: i128) {
        Self::keep_alive(&env);
        let admin: Address = env.storage().instance().get(&LpKey::Admin).unwrap();
        admin.require_auth();
        let balance = Self::read_balance(&env, &to);
        Self::write_balance(&env, &to, balance + amount);
    }

    pub fn balance(env: Env, id: Address) -> i128 {
        Self::keep_alive(&env);
        Self::read_balance(&env, &id)
    }

    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        Self::keep_alive(&env);
        from.require_auth();
        let from_balance = Self::read_balance(&env, &from);
        assert!(from_balance >= amount, "insufficient balance");
        Self::write_balance(&env, &from, from_balance - amount);
        let to_balance = Self::read_balance(&env, &to);
        Self::write_balance(&env, &to, to_balance + amount);
    }

    fn keep_alive(env: &Env) {
        let max_ttl = env.storage().max_ttl();
        env.storage().instance().extend_ttl(max_ttl - 1, max_ttl);
    }

    fn read_balance(env: &Env, id: &Address) -> i128 {
        let key = LpKey::Balance(id.clone());
        let max_ttl = env.storage().max_ttl();
        if env.storage().persistent().has(&key) {
            env.storage()
                .persistent()
                .extend_ttl(&key, max_ttl - 1, max_ttl);
        }
        env.storage().persistent().get(&key).unwrap_or(0)
    }

    fn write_balance(env: &Env, id: &Address, amount: i128) {
        let key = LpKey::Balance(id.clone());
        env.storage().persistent().set(&key, &amount);
        let max_ttl = env.storage().max_ttl();
        env.storage()
            .persistent()
            .extend_ttl(&key, max_ttl - 1, max_ttl);
    }
}

struct Setup {
    env: Env,
    contract_id: Address,
    governance: Address,
    beneficiary: Address,
    pool: Address,
    lp_token: Address,
}

/// Setup with the built-in Stellar asset contract as the LP token.
fn setup() -> Setup {
    let env = Env::default();
    env.mock_all_auths();

    let governance = Address::generate(&env);
    let beneficiary = Address::generate(&env);
    let pool = Address::generate(&env);
    // The pool is the LP token's admin, as an AMM pool is for the LP token it issues.
    let lp_token = env
        .register_stellar_asset_contract_v2(pool.clone())
        .address();

    let contract_id = env.register_contract(None, PolVestingContract);
    PolVestingContractClient::new(&env, &contract_id)
        .initialize(&governance, &Address::generate(&env));
    StellarAssetClient::new(&env, &lp_token).mint(&contract_id, &1_000_000_000);

    Setup {
        env,
        contract_id,
        governance,
        beneficiary,
        pool,
        lp_token,
    }
}

/// Setup with an LP token that keeps itself alive, for the long-schedule tests.
fn setup_keep_alive_lp() -> Setup {
    let env = Env::default();
    env.mock_all_auths();

    let governance = Address::generate(&env);
    let beneficiary = Address::generate(&env);
    let pool = Address::generate(&env);
    let lp_token = env.register_contract(None, KeepAliveLpToken);
    KeepAliveLpTokenClient::new(&env, &lp_token).initialize(&pool);

    let contract_id = env.register_contract(None, PolVestingContract);
    PolVestingContractClient::new(&env, &contract_id)
        .initialize(&governance, &Address::generate(&env));
    KeepAliveLpTokenClient::new(&env, &lp_token).mint(&contract_id, &1_000_000_000);

    Setup {
        env,
        contract_id,
        governance,
        beneficiary,
        pool,
        lp_token,
    }
}

fn client<'a>(s: &'a Setup) -> PolVestingContractClient<'a> {
    PolVestingContractClient::new(&s.env, &s.contract_id)
}

/// Create a schedule from ledger 0 to `end` for the beneficiary.
fn create_until(s: &Setup, end: u32) -> u32 {
    client(s).create_vesting(
        &s.governance,
        &s.beneficiary,
        &s.lp_token,
        &s.pool,
        &TOTAL,
        &0,
        &0,
        &end,
    )
}

fn advance(env: &Env, ledgers: u32) {
    env.ledger().with_mut(|l| l.sequence_number += ledgers);
}

#[test]
fn create_rejects_pool_that_did_not_issue_the_lp_token() {
    let s = setup();
    let other_pool = Address::generate(&s.env);

    let err = client(&s)
        .try_create_vesting(
            &s.governance,
            &s.beneficiary,
            &s.lp_token,
            &other_pool,
            &TOTAL,
            &0,
            &0,
            &1000,
        )
        .unwrap_err()
        .unwrap();

    assert_eq!(err, VestingError::LpTokenMismatch);
}

#[test]
fn create_rejects_an_address_that_is_not_an_lp_token() {
    let s = setup();
    let not_a_token = Address::generate(&s.env);

    let err = client(&s)
        .try_create_vesting(
            &s.governance,
            &s.beneficiary,
            &not_a_token,
            &s.pool,
            &TOTAL,
            &0,
            &0,
            &1000,
        )
        .unwrap_err()
        .unwrap();

    assert_eq!(err, VestingError::LpTokenMismatch);
}

#[test]
fn create_accepts_lp_token_issued_by_the_pool() {
    let s = setup();

    let id = create_until(&s, 1000);

    assert_eq!(id, 0);
    assert_eq!(client(&s).committed(&s.lp_token), TOTAL);
}

#[test]
fn rejected_pool_mismatch_leaves_no_schedule_or_commitment() {
    let s = setup();
    let other_pool = Address::generate(&s.env);

    let _ = client(&s).try_create_vesting(
        &s.governance,
        &s.beneficiary,
        &s.lp_token,
        &other_pool,
        &TOTAL,
        &0,
        &0,
        &1000,
    );

    assert_eq!(client(&s).schedule_count(&s.beneficiary), 0);
    assert_eq!(client(&s).committed(&s.lp_token), 0);
}

#[test]
fn initialize_emits_initialized_event_with_configuration() {
    let env = Env::default();
    let governance = Address::generate(&env);
    let treasury = Address::generate(&env);
    let contract_id = env.register_contract(None, PolVestingContract);

    PolVestingContractClient::new(&env, &contract_id).initialize(&governance, &treasury);

    let events = env.events().all();
    let (_, topics, data) = events
        .iter()
        .rev()
        .find(|(contract, _, _)| *contract == contract_id)
        .expect("initialize published no event");
    let topic = Symbol::try_from_val(&env, &topics.get(0).unwrap()).unwrap();
    assert_eq!(topic, Symbol::new(&env, "initialized"));
    let payload = <(u32, (Address, Address))>::try_from_val(&env, &data).unwrap();
    assert_eq!(
        payload,
        (
            soroban_amm_sdk::EVENT_SCHEMA_VERSION,
            (governance, treasury)
        )
    );
}

#[test]
fn initialize_does_not_require_authorization() {
    // No `mock_all_auths`: any `require_auth` in `initialize` would panic here.
    // The stored addresses are checked by the deploy script instead.
    let env = Env::default();
    let governance = Address::generate(&env);
    let treasury = Address::generate(&env);
    let contract_id = env.register_contract(None, PolVestingContract);
    let client = PolVestingContractClient::new(&env, &contract_id);

    client.initialize(&governance, &treasury);

    assert_eq!(client.get_governance(), governance);
    assert_eq!(client.get_treasury(), treasury);
}

#[test]
fn upgrade_rejects_a_caller_that_is_not_governance() {
    let s = setup();
    let outsider = Address::generate(&s.env);
    let new_hash = BytesN::from_array(&s.env, &[7u8; 32]);

    let err = client(&s)
        .try_upgrade(&outsider, &new_hash)
        .unwrap_err()
        .unwrap();

    assert_eq!(err, VestingError::NotGovernance);
}

#[test]
fn schedule_survives_past_the_claim_window_without_any_reads() {
    // A schedule ending after `BUMP_TO` ledgers (about 180 days) must not
    // archive before it vests just because nobody touched it. Before the fix
    // the creation TTL stopped at `BUMP_TO`, so this read would find it gone.
    let s = setup();
    let id = create_until(&s, 400 * DAY_LEDGERS);

    advance(&s.env, 190 * DAY_LEDGERS);

    assert!(client(&s).try_get_vesting(&s.beneficiary, &id).is_ok());
}

#[test]
fn views_alone_keep_a_400_day_schedule_alive_until_it_vests() {
    let s = setup_keep_alive_lp();
    let end = 400 * DAY_LEDGERS;
    let id = create_until(&s, end);

    // 390 days pass in 30-day steps. Each step makes only read-only calls: the
    // schedule views, and a balance read on the LP token, which keeps the token
    // alive as a busy token would. Nothing writes to the vesting contract.
    let lp = KeepAliveLpTokenClient::new(&s.env, &s.lp_token);
    for _ in 0..13 {
        advance(&s.env, 30 * DAY_LEDGERS);
        let c = client(&s);
        c.get_vesting(&s.beneficiary, &id);
        c.vested_amount(&s.beneficiary, &id);
        c.releasable_amount(&s.beneficiary, &id);
        lp.balance(&s.contract_id);
    }

    // Day 400: the schedule is fully vested and `release` still works.
    advance(&s.env, 10 * DAY_LEDGERS);
    assert_eq!(s.env.ledger().sequence(), end);
    let released = client(&s).release(&s.beneficiary, &id);
    assert_eq!(released, TOTAL);
}

#[test]
fn schedule_ending_at_the_maximum_ledger_is_created() {
    // The TTL clamp must hold even for the largest representable end ledger.
    let s = setup();

    let id = create_until(&s, u32::MAX);

    assert_eq!(id, 0);
    assert!(client(&s).try_get_vesting(&s.beneficiary, &id).is_ok());
}

#[test]
fn rejected_change_beneficiary_at_the_cap_leaves_state_unchanged() {
    let s = setup();
    let destination = Address::generate(&s.env);
    // Fill the destination to the schedule cap.
    for _ in 0..MAX_SCHEDULES_PER_BENEFICIARY {
        client(&s).create_vesting(
            &s.governance,
            &destination,
            &s.lp_token,
            &s.pool,
            &1,
            &0,
            &0,
            &1000,
        );
    }
    let id = create_until(&s, 1000);
    let before = client(&s).get_vesting(&s.beneficiary, &id);
    let source_count = client(&s).schedule_count(&s.beneficiary);
    let destination_count = client(&s).schedule_count(&destination);

    let err = client(&s)
        .try_change_beneficiary(&s.governance, &s.beneficiary, &id, &destination)
        .unwrap_err()
        .unwrap();

    assert_eq!(err, VestingError::TooManySchedules);
    assert_eq!(client(&s).get_vesting(&s.beneficiary, &id), before);
    assert_eq!(client(&s).schedule_count(&s.beneficiary), source_count);
    assert_eq!(client(&s).schedule_count(&destination), destination_count);
}
