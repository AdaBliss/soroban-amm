#![allow(dead_code)]

use crate::{ConcentratedLiquidity, ConcentratedLiquidityClient};
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
    client.initialize(&admin, &token_a, &token_b, &30_i128, &100_i32, &1_i32);

    // Provide wide liquidity
    let lp = Address::generate(&env);
    StellarAssetClient::new(&env, &token_a).mint(&lp, &100_000_000);
    StellarAssetClient::new(&env, &token_b).mint(&lp, &100_000_000);
    client.mint_position(
        &lp,
        &-1000,
        &1000,
        &10_000_000,
        &10_000_000,
        &0,
        &0,
        &u64::MAX,
    );

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
fn test_quote_exact_out_zero_limit_matches_max_limit_for_one_for_zero() {
    let te = setup_test();

    // zero_for_one = false, desired output = 100
    let quote_with_zero = te.client.quote_exact_out(&false, &100, &0);
    let quote_with_max = te.client.quote_exact_out(&false, &100, &u128::MAX);

    assert_eq!(quote_with_zero, quote_with_max);
    assert!(quote_with_zero > 0);
}

#[test]
fn test_swap_exact_out_zero_limit_matches_max_limit_for_one_for_zero() {
    let te = setup_test();
    let trader = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_b).mint(&trader, &100_000);

    // zero_for_one = false with sqrt_price_limit_x96 == 0 should not halt immediately
    let inp = te
        .client
        .swap_exact_out(&trader, &false, &100, &0, &100_000, &u64::MAX);
    assert!(inp > 0);
}

#[test]
fn test_swap_exact_out_records_oracle_point() {
    let te = setup_test();
    let trader = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&trader, &100_000);

    te.env.ledger().set_timestamp(1_100);
    te.client
        .swap_exact_out(&trader, &true, &50, &0, &100_000, &u64::MAX);

    te.env.ledger().set_timestamp(1_200);
    te.client
        .swap_exact_out(&trader, &true, &50, &0, &100_000, &u64::MAX);

    // TWAP observe over 100 seconds reads through exact out swaps
    let obs = te.client.observe(&100);
    assert!(obs > 0);
}

#[test]
fn test_exact_in_and_exact_out_parity_grid() {
    let te = setup_test();
    let trader = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&trader, &1_000_000);
    StellarAssetClient::new(&te.env, &te.token_b).mint(&trader, &1_000_000);

    for zero_for_one in [true, false] {
        let amount_out = 200_i128;
        let quoted_in = te.client.quote_exact_out(&zero_for_one, &amount_out, &0);
        assert!(quoted_in > 0);

        let actual_in = te.client.swap_exact_out(
            &trader,
            &zero_for_one,
            &amount_out,
            &0,
            &(quoted_in * 2),
            &u64::MAX,
        );
        assert_eq!(actual_in, quoted_in);
    }
}
