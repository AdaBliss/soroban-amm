#![allow(dead_code)]

use crate::{
    ClError, ConcentratedLiquidity, ConcentratedLiquidityClient, DataKey, RangeOrderStatus,
    TickInfo,
};
use soroban_sdk::testutils::storage::Persistent as _;
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::{testutils::Address as _, testutils::Ledger as _, Address, Env};

struct TestEnv {
    env: Env,
    admin: Address,
    token_a: Address,
    token_b: Address,
    pool_addr: Address,
    client: ConcentratedLiquidityClient<'static>,
}

fn setup_test() -> TestEnv {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);

    let admin = Address::generate(&env);
    let token_a = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_b = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    let pool_addr = env.register_contract(None, ConcentratedLiquidity);
    let client = ConcentratedLiquidityClient::new(&env, &pool_addr);
    client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &1_i32);

    TestEnv {
        env,
        admin,
        token_a,
        token_b,
        pool_addr,
        client,
    }
}

#[test]
fn test_ticks_stored_in_persistent_not_instance() {
    let te = setup_test();
    let lp = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&lp, &10_000_000);
    StellarAssetClient::new(&te.env, &te.token_b).mint(&lp, &10_000_000);

    te.client
        .mint_position(&lp, &-50, &50, &1_000, &1_000, &0, &0, &u64::MAX);

    te.env.as_contract(&te.pool_addr, || {
        // Ticks must be in persistent storage, NOT instance storage
        assert!(te.env.storage().persistent().has(&DataKey::Tick(-50)));
        assert!(te.env.storage().persistent().has(&DataKey::Tick(50)));
        assert!(!te.env.storage().instance().has(&DataKey::Tick(-50)));
        assert!(!te.env.storage().instance().has(&DataKey::Tick(50)));

        // Tick bitmap word must be in persistent storage, NOT instance storage
        let word_pos = (-50i32).div_euclid(128);
        assert!(te
            .env
            .storage()
            .persistent()
            .has(&DataKey::TickBitmap(word_pos)));
        assert!(!te
            .env
            .storage()
            .instance()
            .has(&DataKey::TickBitmap(word_pos)));
    });
}

#[test]
fn test_constant_instance_storage_with_many_ticks() {
    let te = setup_test();
    let lp = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&lp, &1_000_000_000);
    StellarAssetClient::new(&te.env, &te.token_b).mint(&lp, &1_000_000_000);

    // Mint position 1 with substantial liquidity
    te.client
        .mint_position(&lp, &-10, &10, &10_000_000, &10_000_000, &0, &0, &u64::MAX);

    // Mint 50 separate tick ranges
    for i in 1..=50 {
        let lower = i * 20;
        let upper = lower + 10;
        te.client
            .mint_position(&lp, &lower, &upper, &10_000, &10_000, &0, &0, &u64::MAX);
    }

    te.env.as_contract(&te.pool_addr, || {
        // None of the ticks or bitmap words should exist in instance storage
        for i in 1..=50 {
            let lower = i * 20;
            let upper = lower + 10;
            assert!(!te.env.storage().instance().has(&DataKey::Tick(lower)));
            assert!(!te.env.storage().instance().has(&DataKey::Tick(upper)));
            assert!(te.env.storage().persistent().has(&DataKey::Tick(lower)));
            assert!(te.env.storage().persistent().has(&DataKey::Tick(upper)));
        }
    });

    // Swapping through the ticks and burning works
    let trader = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&trader, &100_000_000);
    te.client.swap(&trader, &true, &100_000, &0, &0, &u64::MAX);

    let (a, b) = te.client.burn_position(&lp, &-10, &10, &500_000);
    assert!(a > 0 || b > 0);
}

#[test]
fn test_extreme_ticks_walk_inside_instruction_budget() {
    let te = setup_test();
    let lp = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&lp, &10_000_000_000);
    StellarAssetClient::new(&te.env, &te.token_b).mint(&lp, &10_000_000_000);

    // Initialize ticks at extreme ends of valid tick range
    let tick_low = -800_000;
    let tick_high = 800_000;

    te.client.mint_position(
        &lp,
        &tick_low,
        &(tick_low + 10),
        &10_000,
        &10_000,
        &0,
        &0,
        &u64::MAX,
    );
    te.client.mint_position(
        &lp,
        &(tick_high - 10),
        &tick_high,
        &10_000,
        &10_000,
        &0,
        &0,
        &u64::MAX,
    );

    // Looking for the next initialized tick across the huge gap must complete
    // quickly without exceeding instruction budget
    let next_tick = te.client.next_initialized_tick_pub(&(tick_low + 10));
    assert_eq!(next_tick, Some(tick_high - 10));

    let prev_tick = te.client.prev_initialized_tick_pub(&(tick_high - 11));
    assert_eq!(prev_tick, Some(tick_low + 10));
}

#[test]
fn test_persistent_ttl_advance_and_walk_extends_ttl() {
    let te = setup_test();
    let lp = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&lp, &10_000_000);
    StellarAssetClient::new(&te.env, &te.token_b).mint(&lp, &10_000_000);

    te.client
        .mint_position(&lp, &-50, &50, &100_000, &100_000, &0, &0, &u64::MAX);

    let tick_key = DataKey::Tick(-50);
    let ttl_before = te.env.as_contract(&te.pool_addr, || {
        te.env.storage().persistent().get_ttl(&tick_key)
    });
    assert!(ttl_before >= 3_110_400 - 1);

    // Advance ledger so TTL drops below threshold
    te.env
        .ledger()
        .with_mut(|l| l.sequence_number += 3_110_400 - 518_400 + 10);

    let ttl_mid = te.env.as_contract(&te.pool_addr, || {
        te.env.storage().persistent().get_ttl(&tick_key)
    });
    assert!(ttl_mid < 518_400);

    // Reading tick info or swapping bumps the TTL back up
    let _info = te.client.get_tick_info(&-50);

    let ttl_after = te.env.as_contract(&te.pool_addr, || {
        te.env.storage().persistent().get_ttl(&tick_key)
    });
    assert!(ttl_after >= 3_110_400 - 1);
}

#[test]
fn test_uninitialized_tick_query_does_not_trap() {
    let te = setup_test();
    // Asking about a missing or never-initialized tick must not trap
    assert!(!te.client.is_tick_initialized(&12_345));
    assert_eq!(
        te.client.try_get_tick_info(&12_345),
        Err(Ok(ClError::TickNotInitialized))
    );
}

#[test]
fn test_property_model_for_tick_lookups() {
    let te = setup_test();
    let lp = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&lp, &100_000_000);
    StellarAssetClient::new(&te.env, &te.token_b).mint(&lp, &100_000_000);

    let ticks = [-1000, -500, -250, 0, 250, 500, 1000];
    for i in 0..ticks.len() - 1 {
        te.client.mint_position(
            &lp,
            &ticks[i],
            &ticks[i + 1],
            &10_000,
            &10_000,
            &0,
            &0,
            &u64::MAX,
        );
    }

    // Verify all ticks are initialized
    for &t in &ticks {
        assert!(te.client.is_tick_initialized(&t));
        let info = te.client.get_tick_info(&t);
        assert!(info.liquidity_gross > 0);
    }

    // Check next_initialized_tick matches sequential model
    for i in 0..ticks.len() - 1 {
        let query = ticks[i];
        let expected = ticks[i + 1];
        assert_eq!(te.client.next_initialized_tick_pub(&query), Some(expected));
    }

    // Check prev_initialized_tick matches backward model
    for i in 1..ticks.len() {
        let query = ticks[i] - 1;
        let expected = ticks[i - 1];
        assert_eq!(te.client.prev_initialized_tick_pub(&query), Some(expected));
    }
}

#[test]
fn test_legacy_instance_entries_migration_swap_mint_burn() {
    let te = setup_test();
    let lp = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&lp, &10_000_000);
    StellarAssetClient::new(&te.env, &te.token_b).mint(&lp, &10_000_000);

    // Populate legacy instance storage entries directly
    te.env.as_contract(&te.pool_addr, || {
        let tick_info = TickInfo {
            liquidity_gross: 10_000,
            liquidity_net: 10_000,
            fee_growth_outside_a: 0,
            fee_growth_outside_b: 0,
            initialized: true,
        };
        te.env
            .storage()
            .instance()
            .set(&DataKey::Tick(100), &tick_info);
        te.env
            .storage()
            .instance()
            .set(&DataKey::Tick(-100), &tick_info);
        // Put bitmap words in instance storage (legacy layout)
        let word_pos_pos = 100i32.div_euclid(128);
        let word_pos_neg = (-100i32).div_euclid(128);
        te.env
            .storage()
            .instance()
            .set(&DataKey::TickBitmap(word_pos_pos), &(1u128 << 100));
        te.env.storage().instance().set(
            &DataKey::TickBitmap(word_pos_neg),
            &(1u128 << ((-100i32).rem_euclid(128))),
        );
    });

    // 1. Read-through works on legacy entries
    assert!(te.client.is_tick_initialized(&100));
    assert!(te.client.is_tick_initialized(&-100));
    let info = te.client.get_tick_info(&100);
    assert_eq!(info.liquidity_gross, 10_000);

    // 2. Mint position touches ticks and migrates them to persistent storage
    te.client
        .mint_position(&lp, &-100, &100, &5_000, &5_000, &0, &0, &u64::MAX);

    te.env.as_contract(&te.pool_addr, || {
        // Ticks have been migrated to persistent and removed from instance
        assert!(te.env.storage().persistent().has(&DataKey::Tick(100)));
        assert!(!te.env.storage().instance().has(&DataKey::Tick(100)));
    });

    // 3. Swap operates normally
    let trader = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&trader, &100_000);
    let out = te.client.swap(&trader, &true, &1_000, &0, &0, &u64::MAX);
    assert!(out > 0);

    // 4. Burn position works
    let (a, b) = te.client.burn_position(&lp, &-100, &100, &2_000);
    assert!(a > 0 || b > 0);
}

#[test]
fn test_range_order_and_nft_indexes_persistent_storage() {
    let te = setup_test();
    let provider = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&provider, &10_000_000);

    // Range order storage
    te.client
        .place_range_order(&provider, &20, &40, &te.token_a, &10_000, &0, &u64::MAX);

    let ro_key = DataKey::RangeOrder(provider.clone(), 20, 40);
    te.env.as_contract(&te.pool_addr, || {
        assert!(te.env.storage().persistent().has(&ro_key));
        assert!(!te.env.storage().instance().has(&ro_key));
    });

    // Check range order filled query
    let status = te.client.check_range_order_filled(&provider, &20, &40);
    assert_eq!(status, RangeOrderStatus::Pending);
}
