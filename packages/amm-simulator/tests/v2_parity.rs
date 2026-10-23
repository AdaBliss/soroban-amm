//! Deterministic V2 arithmetic parity against the deployed AMM contract.
//! All comparisons are integer exact (zero tolerance).
use amm::{AmmPool, AmmPoolClient};
use soroban_amm_simulator::PoolState;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::StellarAssetClient,
    Address, Env,
};
use token::LpToken;

fn run_case(
    fee: i128,
    protocol: i128,
    rebate: i128,
    seed: u64,
    initial: i128,
    max_input: i128,
    one_sided: bool,
) {
    let env = Env::default();
    env.budget().reset_unlimited();
    env.mock_all_auths();
    env.ledger().set_timestamp(100);
    let admin = Address::generate(&env);
    let lp_addr = env.register_contract(None, LpToken);
    let amm_addr = env.register_contract(None, AmmPool);
    token::LpTokenClient::new(&env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(&env, "LP"),
        &soroban_sdk::String::from_str(&env, "LP"),
        &7,
    );
    let ta = env.register_stellar_asset_contract_v2(admin.clone());
    let tb = env.register_stellar_asset_contract_v2(admin.clone());
    let a = ta.address();
    let b = tb.address();
    let asset_a = StellarAssetClient::new(&env, &a);
    let asset_b = StellarAssetClient::new(&env, &b);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(&admin, &a, &b, &lp_addr, &fee, &admin, &protocol);
    let lp = Address::generate(&env);
    asset_a.mint(&lp, &1_000_000_000_000_i128);
    asset_b.mint(&lp, &1_000_000_000_000_i128);
    amm.add_liquidity(&lp, &initial, &initial, &1, &u64::MAX);
    let info = amm.get_info();
    let mut sim = PoolState {
        token_a: "A".into(),
        token_b: "B".into(),
        reserve_a: info.reserve_a,
        reserve_b: info.reserve_b,
        total_shares: info.total_shares,
        fee_bps: fee,
        protocol_fee_bps: protocol,
        lp_rebate_bps: 0,
        accrued_fee_a: 0,
        accrued_fee_b: 0,
        price_cumulative_a: 0,
        price_cumulative_b: 0,
        last_timestamp: 100,
        paused: false,
        min_liquidity_locked: true,
        locked_liquidity: 1_000,
    };
    sim.lp_rebate_bps = rebate;
    amm.set_lp_rebate(&admin, &rebate);
    let trader = Address::generate(&env);
    asset_a.mint(&trader, &1_000_000_000_000_000_i128);
    asset_b.mint(&trader, &1_000_000_000_000_000_i128);
    let mut rng = seed;
    for step in 0..300 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let side_a = rng & 1 == 0;
        let token_in = if side_a { a.clone() } else { b.clone() };
        let token_in_name = if side_a { "A" } else { "B" };
        let input = 1 + ((rng >> 16) as i128 % max_input);
        if step == 100 {
            let expected = sim.quote_add_liquidity(100_000, 100_000).unwrap();
            asset_a.mint(&lp, &100_000);
            asset_b.mint(&lp, &100_000);
            let minted = amm.add_liquidity(&lp, &100_000, &100_000, &1, &u64::MAX);
            assert_eq!(minted, expected.shares, "add-liquidity fee={fee}");
            sim.execute_add_liquidity(100_000, 100_000, 1).unwrap();
        } else if step == 150 && one_sided {
            let shares = 10_000;
            let token_out = if side_a { a.clone() } else { b.clone() };
            let token_out_name = if side_a { "A" } else { "B" };
            let expected = sim
                .execute_remove_liquidity_one_sided(shares, token_out_name, 0)
                .unwrap();
            let actual = amm.remove_liquidity_one_sided(&lp, &shares, &token_out, &0, &u64::MAX);
            assert_eq!(actual, expected.total_out, "one-sided output fee={fee}");
        } else if step == 150 {
            // The V2 contract has a runtime LP-rebate setter, but no swap-fee
            // setter. Exercise that live configuration change on both sides.
            let updated_rebate = if rebate == 5_000 { 2_500 } else { 5_000 };
            amm.set_lp_rebate(&admin, &updated_rebate);
            sim.lp_rebate_bps = updated_rebate;
        } else if step == 200 {
            let shares = 100_000;
            let expected = sim.quote_remove_liquidity(shares).unwrap();
            let withdrawn = amm.remove_liquidity(&lp, &shares, &0, &0, &u64::MAX);
            assert_eq!(
                withdrawn,
                (expected.amount_a, expected.amount_b),
                "remove-liquidity fee={fee}"
            );
            sim.execute_remove_liquidity(shares, 0, 0).unwrap();
        } else if step % 3 == 0 {
            let token_out = if side_a { b.clone() } else { a.clone() };
            let token_out_name = if side_a { "B" } else { "A" };
            let divisor = if fee == 9_999 { 1_000_000 } else { 10_000 };
            let amount_out = (if side_a { sim.reserve_b } else { sim.reserve_a }) / divisor;
            let expected = sim
                .quote_swap_exact_out(token_out_name, amount_out)
                .unwrap();
            let paid = amm.swap_exact_out(&trader, &token_out, &amount_out, &i128::MAX, &u64::MAX);
            assert_eq!(paid, expected.amount_in, "exact-out fee={fee}, step={step}");
            sim.execute_swap_exact_out(token_out_name, amount_out, i128::MAX)
                .unwrap();
        } else {
            let expected = sim.quote_swap_exact_in(token_in_name, input).unwrap();
            let got = amm.swap(&trader, &token_in, &input, &0, &u64::MAX);
            assert_eq!(got, expected.amount_out, "exact-in fee={fee}, step={step}");
            sim.execute_swap_exact_in(token_in_name, input, 0).unwrap();
        }
        let info = amm.get_info();
        assert_eq!(
            info.reserve_a, sim.reserve_a,
            "reserve A fee={fee}, step={step}"
        );
        assert_eq!(
            info.reserve_b, sim.reserve_b,
            "reserve B fee={fee}, step={step}"
        );
        assert_eq!(info.total_shares, sim.total_shares);
        let (fee_a, fee_b) = amm.get_accrued_fees();
        assert_eq!((fee_a, fee_b), (sim.accrued_fee_a, sim.accrued_fee_b));
    }
}

#[test]
fn parity_fee_zero() {
    run_case(0, 0, 0, 1, 1_000_000_000_000, 900_000, false);
}
#[test]
fn parity_fee_one_with_rebate() {
    // protocol_fee_bps must stay strictly below fee_bps (#1107), so a 1 bps
    // swap fee carries no protocol cut.
    run_case(1, 0, 10_000, 2, 1_000_000_000_000, 900_000, false);
}
#[test]
fn parity_fee_thirty_with_rebate() {
    run_case(30, 10, 5_000, 3, 1_000_000_000_000, 900_000, false);
}
#[test]
fn parity_fee_9999_edge() {
    run_case(9_999, 0, 0, 4, 1_000_000_000_000, 900_000, false);
}
#[test]
fn parity_tiny_reserves() {
    run_case(0, 0, 0, 5, 100_000, 1_000, false);
}
#[test]
#[ignore = "enable after AMM one-sided reserve-integrity issue #1093 is resolved"]
fn parity_one_sided_removal_after_contract_fix() {
    run_case(30, 10, 5_000, 6, 1_000_000_000_000, 900_000, true);
}
