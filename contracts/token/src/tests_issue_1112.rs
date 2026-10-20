#![allow(dead_code)]

extern crate std;

use crate::{LpToken, LpTokenClient, TokenError};
use soroban_sdk::{
    testutils::Address as _, testutils::Events as _, testutils::Ledger as _, Address, Env, IntoVal,
    String, Symbol, TryFromVal, Val,
};

struct TestSetup {
    env: Env,
    admin: Address,
    contract_addr: Address,
    client: LpTokenClient<'static>,
}

fn setup_token() -> TestSetup {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);

    let admin = Address::generate(&env);
    let contract_addr = env.register_contract(None, LpToken);
    let client = LpTokenClient::new(&env, &contract_addr);
    client.initialize(
        &admin,
        &String::from_str(&env, "LP Token"),
        &String::from_str(&env, "LPT"),
        &7,
    );

    TestSetup {
        env,
        admin,
        contract_addr,
        client,
    }
}

fn last_versioned_event<T>(ts: &TestSetup, topic: &str) -> (u32, T)
where
    T: TryFromVal<Env, Val>,
{
    let wanted = Symbol::new(&ts.env, topic);
    let evt = ts
        .env
        .events()
        .all()
        .iter()
        .rfind(|e| {
            e.0 == ts.contract_addr
                && e.1
                    .get(0)
                    .map(|t| Symbol::try_from_val(&ts.env, &t) == Ok(wanted.clone()))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("no `{topic}` event found"));
    evt.2.into_val(&ts.env)
}

fn last_raw_event(ts: &TestSetup, topic: &str) -> (soroban_sdk::Vec<Val>, Val) {
    let wanted = Symbol::new(&ts.env, topic);
    let evt = ts
        .env
        .events()
        .all()
        .iter()
        .rfind(|e| {
            e.0 == ts.contract_addr
                && e.1
                    .get(0)
                    .map(|t| Symbol::try_from_val(&ts.env, &t) == Ok(wanted.clone()))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("no `{topic}` event found"));
    (evt.1, evt.2)
}

#[test]
fn test_views_before_initialize_return_not_initialized() {
    let env = Env::default();
    let contract_addr = env.register_contract(None, LpToken);
    let client = LpTokenClient::new(&env, &contract_addr);

    assert_eq!(client.try_name(), Err(Ok(TokenError::NotInitialized)));
    assert_eq!(client.try_symbol(), Err(Ok(TokenError::NotInitialized)));
    assert_eq!(client.try_decimals(), Err(Ok(TokenError::NotInitialized)));
    assert_eq!(client.try_admin(), Err(Ok(TokenError::NotInitialized)));
    assert_eq!(client.try_locker(), Err(Ok(TokenError::NotInitialized)));
}

#[test]
fn test_initialize_emits_versioned_event() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let contract_addr = env.register_contract(None, LpToken);
    let client = LpTokenClient::new(&env, &contract_addr);

    client.initialize(
        &admin,
        &String::from_str(&env, "Token Name"),
        &String::from_str(&env, "TKN"),
        &7,
    );

    let wanted = Symbol::new(&env, "initialized");
    let evt = env
        .events()
        .all()
        .iter()
        .rfind(|e| {
            e.0 == contract_addr
                && e.1
                    .get(0)
                    .map(|t| Symbol::try_from_val(&env, &t) == Ok(wanted.clone()))
                    .unwrap_or(false)
        })
        .expect("no initialized event found");

    let (ver, (evt_admin, name, sym, dec)): (u32, (Address, String, String, u32)) =
        evt.2.into_val(&env);
    assert_eq!(ver, 1);
    assert_eq!(evt_admin, admin);
    assert_eq!(name, String::from_str(&env, "Token Name"));
    assert_eq!(sym, String::from_str(&env, "TKN"));
    assert_eq!(dec, 7);
}

#[test]
fn test_sep41_mint_event() {
    let ts = setup_token();
    let alice = Address::generate(&ts.env);

    ts.client.mint(&alice, &500);

    let (topics, data) = last_raw_event(&ts, "mint");
    assert_eq!(topics.len(), 3);
    let admin_evt: Address = Address::try_from_val(&ts.env, &topics.get(1).unwrap()).unwrap();
    let to_evt: Address = Address::try_from_val(&ts.env, &topics.get(2).unwrap()).unwrap();
    let amount: i128 = i128::try_from_val(&ts.env, &data).unwrap();

    assert_eq!(admin_evt, ts.admin);
    assert_eq!(to_evt, alice);
    assert_eq!(amount, 500);
}

#[test]
fn test_sep41_burn_event() {
    let ts = setup_token();
    let alice = Address::generate(&ts.env);

    ts.client.mint(&alice, &500);
    ts.client.burn(&alice, &200);

    let (topics, data) = last_raw_event(&ts, "burn");
    assert_eq!(topics.len(), 2);
    let from_evt: Address = Address::try_from_val(&ts.env, &topics.get(1).unwrap()).unwrap();
    let amount: i128 = i128::try_from_val(&ts.env, &data).unwrap();

    assert_eq!(from_evt, alice);
    assert_eq!(amount, 200);
}

#[test]
fn test_sep41_approve_event() {
    let ts = setup_token();
    let alice = Address::generate(&ts.env);
    let bob = Address::generate(&ts.env);

    ts.client.approve(&alice, &bob, &300, &2_000);

    let (topics, data) = last_raw_event(&ts, "approve");
    assert_eq!(topics.len(), 3);
    let from_evt: Address = Address::try_from_val(&ts.env, &topics.get(1).unwrap()).unwrap();
    let spender_evt: Address = Address::try_from_val(&ts.env, &topics.get(2).unwrap()).unwrap();
    let (amount, live_until): (i128, u32) = <(i128, u32)>::try_from_val(&ts.env, &data).unwrap();

    assert_eq!(from_evt, alice);
    assert_eq!(spender_evt, bob);
    assert_eq!(amount, 300);
    assert_eq!(live_until, 2_000);
}

#[test]
fn test_sep41_transfer_from_emits_approve_and_transfer_events() {
    let ts = setup_token();
    let alice = Address::generate(&ts.env);
    let bob = Address::generate(&ts.env);
    let charlie = Address::generate(&ts.env);

    ts.client.mint(&alice, &1000);
    ts.client.approve(&alice, &bob, &500, &2_000);

    ts.client.transfer_from(&bob, &alice, &charlie, &200);

    // Assert transfer event
    let (t_topics, t_data) = last_raw_event(&ts, "transfer");
    assert_eq!(t_topics.len(), 3);
    let from: Address = Address::try_from_val(&ts.env, &t_topics.get(1).unwrap()).unwrap();
    let to: Address = Address::try_from_val(&ts.env, &t_topics.get(2).unwrap()).unwrap();
    let amt: i128 = i128::try_from_val(&ts.env, &t_data).unwrap();
    assert_eq!(from, alice);
    assert_eq!(to, charlie);
    assert_eq!(amt, 200);

    // Assert approve event reflecting decremented allowance
    let (a_topics, a_data) = last_raw_event(&ts, "approve");
    assert_eq!(a_topics.len(), 3);
    let (remaining_allowance, _): (i128, u32) =
        <(i128, u32)>::try_from_val(&ts.env, &a_data).unwrap();
    assert_eq!(remaining_allowance, 300);
}

#[test]
fn test_set_locker_emits_versioned_event() {
    let ts = setup_token();
    let locker = Address::generate(&ts.env);

    ts.client.set_locker(&locker);

    let (ver, (admin_evt, locker_evt)): (u32, (Address, Address)) =
        last_versioned_event(&ts, "set_locker");
    assert_eq!(ver, 1);
    assert_eq!(admin_evt, ts.admin);
    assert_eq!(locker_evt, locker);
}

#[test]
fn test_lock_and_unlock_emit_versioned_events() {
    let ts = setup_token();
    let locker = Address::generate(&ts.env);
    let alice = Address::generate(&ts.env);

    ts.client.set_locker(&locker);
    ts.client.mint(&alice, &1000);

    ts.client.lock(&alice, &400);

    let (ver, (holder, lkr, amt, new_entry, new_locked)): (
        u32,
        (Address, Address, i128, i128, i128),
    ) = last_versioned_event(&ts, "lock");
    assert_eq!(ver, 1);
    assert_eq!(holder, alice);
    assert_eq!(lkr, locker);
    assert_eq!(amt, 400);
    assert_eq!(new_entry, 400);
    assert_eq!(new_locked, 400);

    ts.client.unlock(&alice, &locker, &150);

    let (ver, (holder, lkr, amt, new_entry, new_locked)): (
        u32,
        (Address, Address, i128, i128, i128),
    ) = last_versioned_event(&ts, "unlock");
    assert_eq!(ver, 1);
    assert_eq!(holder, alice);
    assert_eq!(lkr, locker);
    assert_eq!(amt, 150);
    assert_eq!(new_entry, 250);
    assert_eq!(new_locked, 250);
}

#[test]
fn test_migrate_legacy_lock_emits_versioned_event() {
    let ts = setup_token();
    let locker = Address::generate(&ts.env);
    let alice = Address::generate(&ts.env);

    ts.client.set_locker(&locker);
    ts.client.mint(&alice, &1000);

    // Set legacy lock in storage
    ts.env.as_contract(&ts.contract_addr, || {
        ts.env
            .storage()
            .persistent()
            .set(&crate::DataKey::Locked(alice.clone()), &500_i128);
    });

    ts.client.migrate_legacy_lock(&alice, &locker, &500);

    let (ver, (holder, lkr, amt, new_entry, grand_total)): (
        u32,
        (Address, Address, i128, i128, i128),
    ) = last_versioned_event(&ts, "migrate_legacy_lock");
    assert_eq!(ver, 1);
    assert_eq!(holder, alice);
    assert_eq!(lkr, locker);
    assert_eq!(amt, 500);
    assert_eq!(new_entry, 500);
    assert_eq!(grand_total, 500);
}

#[test]
#[allow(clippy::mutable_key_type)]
fn test_replay_accumulator_from_events_matches_contract_state() {
    let ts = setup_token();
    let a = Address::generate(&ts.env);
    let b = Address::generate(&ts.env);
    let c = Address::generate(&ts.env);

    // Sequence of operations
    ts.client.mint(&a, &1000);
    ts.client.mint(&b, &2000);
    ts.client.transfer(&a, &b, &300);
    ts.client.transfer(&b, &c, &500);
    ts.client.burn(&c, &100);
    ts.client.mint(&c, &400);
    ts.client.transfer(&b, &a, &150);
    ts.client.burn(&a, &50);

    // Replay accumulator: reconstruct state strictly from emitted events
    let mut offchain_balances = std::collections::BTreeMap::<Address, i128>::new();
    let mut offchain_total_supply = 0_i128;

    for evt in ts.env.events().all() {
        if evt.0 != ts.contract_addr {
            continue;
        }
        let topic0 = match Symbol::try_from_val(&ts.env, &evt.1.get(0).unwrap()) {
            Ok(s) => s,
            Err(_) => continue,
        };

        if topic0 == Symbol::new(&ts.env, "mint") {
            let to = Address::try_from_val(&ts.env, &evt.1.get(2).unwrap()).unwrap();
            let amount = i128::try_from_val(&ts.env, &evt.2).unwrap();
            *offchain_balances.entry(to).or_insert(0) += amount;
            offchain_total_supply += amount;
        } else if topic0 == Symbol::new(&ts.env, "burn") {
            let from = Address::try_from_val(&ts.env, &evt.1.get(1).unwrap()).unwrap();
            let amount = i128::try_from_val(&ts.env, &evt.2).unwrap();
            *offchain_balances.entry(from).or_insert(0) -= amount;
            offchain_total_supply -= amount;
        } else if topic0 == Symbol::new(&ts.env, "transfer") {
            let from = Address::try_from_val(&ts.env, &evt.1.get(1).unwrap()).unwrap();
            let to = Address::try_from_val(&ts.env, &evt.1.get(2).unwrap()).unwrap();
            let amount = i128::try_from_val(&ts.env, &evt.2).unwrap();
            *offchain_balances.entry(from).or_insert(0) -= amount;
            *offchain_balances.entry(to).or_insert(0) += amount;
        }
    }

    assert_eq!(ts.client.balance(&a), *offchain_balances.get(&a).unwrap());
    assert_eq!(ts.client.balance(&b), *offchain_balances.get(&b).unwrap());
    assert_eq!(ts.client.balance(&c), *offchain_balances.get(&c).unwrap());
    assert_eq!(ts.client.total_supply(), offchain_total_supply);
}
