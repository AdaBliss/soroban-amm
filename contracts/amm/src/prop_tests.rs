extern crate std;
use super::tests::*;
use super::*;
use proptest::prelude::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    Address, Bytes, Env,
};

proptest! {
    /// Property 1: For any valid first deposit, initial shares (sqrt(a*b)) are always positive.
    #[test]
    fn first_deposit_shares_always_positive(
        a in 1_i128..=100_000_i128,
        b in 1_i128..=100_000_i128,
    ) {
        let shares = AmmPool::sqrt(a * b);
        prop_assert!(shares > 0, "shares={shares} for a={a}, b={b}");
    }

    /// Property 2: Subsequent deposit shares minted are ≤ the proportional amount for each token.
    #[test]
    fn subsequent_deposit_shares_leq_proportional(
        amount_a in 1_i128..=1_000_000_i128,
        amount_b in 1_i128..=1_000_000_i128,
        reserve_a in 1_i128..=1_000_000_i128,
        reserve_b in 1_i128..=1_000_000_i128,
        total_shares in 1_i128..=1_000_000_i128,
    ) {
        let shares_a = amount_a * total_shares / reserve_a;
        let shares_b = amount_b * total_shares / reserve_b;
        let minted = shares_a.min(shares_b);
        prop_assert!(minted <= shares_a, "minted={minted} > shares_a={shares_a}");
        prop_assert!(minted <= shares_b, "minted={minted} > shares_b={shares_b}");
    }

    /// Property 3: For any valid shares ≤ total_shares, remove_liquidity outputs are non-negative.
    #[test]
    fn remove_liquidity_outputs_nonneg(
        shares in 1_i128..=10_000_i128,
        extra in 0_i128..=10_000_i128,
        reserve_a in 0_i128..=1_000_000_i128,
        reserve_b in 0_i128..=1_000_000_i128,
    ) {
        // total_shares >= shares by construction
        let total_shares = shares + extra;
        let out_a = shares * reserve_a / total_shares;
        let out_b = shares * reserve_b / total_shares;
        prop_assert!(out_a >= 0, "out_a={out_a} is negative");
        prop_assert!(out_b >= 0, "out_b={out_b} is negative");
    }

    /// Property 4: get_amount_out output is always strictly less than the output reserve.
    #[test]
    fn amount_out_strictly_lt_reserve(
        amount_in in 1_i128..=100_000_i128,
        reserve_in in 1_i128..=1_000_000_i128,
        reserve_out in 1_i128..=1_000_000_i128,
        fee_bps in 0_i128..=10_000_i128,
    ) {
        let amount_in_with_fee = amount_in * (10_000 - fee_bps);
        let denom = reserve_in * 10_000 + amount_in_with_fee;
        // When fee_bps == 10_000, amount_in_with_fee == 0 → amount_out == 0 < reserve_out.
        let amount_out = if denom == 0 {
            0
        } else {
            amount_in_with_fee * reserve_out / denom
        };
        prop_assert!(
            amount_out < reserve_out,
            "amount_out={amount_out} >= reserve_out={reserve_out}"
        );
    }
}

#[test]
fn test_flash_loan_success_with_repayment() {
    let (env, admin, amm_addr, lp_addr, _) = setup();

    let (ta_client, ta_sac) = create_sac(&env, &admin);
    let (tb_client, tb_sac) = create_sac(&env, &admin);

    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize_with_flash_loan_fee(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
        &50_i128,
    );

    let provider = Address::generate(&env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let receiver_addr = env.register_contract(None, MockFlashLoanReceiver);
    let receiver = MockFlashLoanReceiverClient::new(&env, &receiver_addr);
    receiver.initialize(&amm_addr, &ta_client.address, &tb_client.address, &true);

    ta_sac.mint(&receiver_addr, &1_000_i128);
    tb_sac.mint(&receiver_addr, &1_000_i128);

    let fees = amm.flash_loan(
        &receiver_addr,
        &100_000_i128,
        &50_000_i128,
        &Bytes::new(&env),
    );
    assert_eq!(fees, (500, 250));

    let info = amm.get_info();
    assert_eq!(info.reserve_a, 1_000_500);
    assert_eq!(info.reserve_b, 1_000_250);
    assert_eq!(info.flash_loan_fee_bps, 50);
}

#[test]
fn test_flash_loan_failed_repayment_panics() {
    let (env, admin, amm_addr, lp_addr, _) = setup();

    let (ta_client, ta_sac) = create_sac(&env, &admin);
    let (tb_client, tb_sac) = create_sac(&env, &admin);

    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
    );

    let provider = Address::generate(&env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let receiver_addr = env.register_contract(None, MockFlashLoanReceiver);
    let receiver = MockFlashLoanReceiverClient::new(&env, &receiver_addr);
    receiver.initialize(&amm_addr, &ta_client.address, &tb_client.address, &false);

    let result = amm.try_flash_loan(&receiver_addr, &100_000_i128, &0_i128, &Bytes::new(&env));
    assert_eq!(result, Err(Ok(AmmError::FlashLoanRepaymentFailed)));
}

#[test]
fn test_get_fee_info() {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta_client, _) = create_sac(&env, &admin);
    let (tb_client, _) = create_sac(&env, &admin);

    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
    );

    assert_eq!(amm.get_fee_info(), 30_i128);
    assert_eq!(amm.get_fee_info(), amm.get_info().fee_bps);
}

#[test]
fn test_emergency_withdraw() {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta_client, ta_sac) = create_sac(&env, &admin);
    let (tb_client, tb_sac) = create_sac(&env, &admin);
    let amm = AmmPoolClient::new(&env, &amm_addr);

    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
    );

    let provider = Address::generate(&env);
    ta_sac.mint(&provider, &2_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 2_000_000, 1_000_000).execute();

    let info_before = amm.get_info();
    assert_eq!(info_before.reserve_a, 2_000_000);
    assert_eq!(info_before.reserve_b, 1_000_000);

    let recipient = Address::generate(&env);
    let expected_ts = 99999_u64;
    env.ledger().set_timestamp(expected_ts);

    amm.emergency_withdraw(&recipient);

    let info_after = amm.get_info();
    assert_eq!(info_after.reserve_a, 0);
    assert_eq!(info_after.reserve_b, 0);

    assert_eq!(ta_client.balance(&recipient), 2_000_000);
    assert_eq!(tb_client.balance(&recipient), 1_000_000);

    env.as_contract(&amm_addr, || {
        let ts: u64 = env
            .storage()
            .instance()
            .get(&DataKey::EmergencyWithdrawTimestamp)
            .unwrap();
        let rec: Address = env
            .storage()
            .instance()
            .get(&DataKey::EmergencyWithdrawRecipient)
            .unwrap();
        assert_eq!(ts, expected_ts);
        assert_eq!(rec, recipient);
    });
}

/// Regression test for issue #569.
///
/// After `emergency_withdraw` drains the reserves, `TotalShares` must be reset
/// to 0 so that a subsequent `add_liquidity` call takes the initial-deposit
/// path (geometric-mean shares) rather than dividing by the now-zero reserve,
/// which would cause an unconditional panic.
#[test]
fn test_emergency_withdraw_zeros_total_shares_and_allows_redeposit() {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta_client, ta_sac) = create_sac(&env, &admin);
    let (tb_client, tb_sac) = create_sac(&env, &admin);
    let amm = AmmPoolClient::new(&env, &amm_addr);

    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
    );

    let provider = Address::generate(&env);
    ta_sac.mint(&provider, &4_000_000_i128);
    tb_sac.mint(&provider, &2_000_000_i128);
    AddLiquidity::new(&amm, &provider, 2_000_000, 1_000_000).execute();

    // Sanity: pool has reserves and shares.
    let info = amm.get_info();
    assert_eq!(info.reserve_a, 2_000_000);
    assert_eq!(info.reserve_b, 1_000_000);
    assert!(info.total_shares > 0);

    // Emergency drain.
    let recipient = Address::generate(&env);
    amm.emergency_withdraw(&recipient);

    // After drain: reserves are 0, total_shares is preserved, and pool is in terminal drained state (#1095).
    let info_after = amm.get_info();
    assert_eq!(info_after.reserve_a, 0, "reserve_a not zeroed");
    assert_eq!(info_after.reserve_b, 0, "reserve_b not zeroed");
    assert_eq!(
        info_after.total_shares, info.total_shares,
        "total_shares must remain intact after emergency_withdraw (#1095)"
    );
    assert!(amm.is_drained(), "pool must be marked drained");

    // Re-deposit and redemption are blocked with PoolDrained so old LP tokens cannot drain new deposits.
    let new_provider = Address::generate(&env);
    ta_sac.mint(&new_provider, &2_000_000_i128);
    tb_sac.mint(&new_provider, &1_000_000_i128);
    let res = amm.try_add_liquidity(
        &new_provider,
        &2_000_000_i128,
        &1_000_000_i128,
        &0_i128,
        &u64::MAX,
    );
    assert_eq!(res, Err(Ok(AmmError::PoolDrained)));
}

/// Regression test for issue #1095 — multisig path.
///
/// `exec_multisig_emergency_wd` enters the terminal drained state, preserving
/// `TotalShares` and rejecting post-drain deposits and redemptions with `PoolDrained`.
#[test]
fn test_multisig_emergency_wd_zeros_total_shares_and_allows_redeposit() {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta_client, ta_sac) = create_sac(&env, &admin);
    let (tb_client, tb_sac) = create_sac(&env, &admin);
    let amm = AmmPoolClient::new(&env, &amm_addr);

    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
    );

    // Configure a 1-of-1 multisig so the multisig path is active.
    let signer = Address::generate(&env);
    let mut signers = soroban_sdk::Vec::new(&env);
    signers.push_back(signer.clone());
    amm.set_multisig(&admin, &signers, &1_u32);

    let provider = Address::generate(&env);
    ta_sac.mint(&provider, &4_000_000_i128);
    tb_sac.mint(&provider, &2_000_000_i128);
    AddLiquidity::new(&amm, &provider, 2_000_000, 1_000_000).execute();

    let info = amm.get_info();
    assert!(info.total_shares > 0);

    // Propose + execute the multisig emergency withdrawal.
    let recipient = Address::generate(&env);
    amm.propose_emergency_withdraw(&signer, &recipient);
    amm.exec_multisig_emergency_wd(&signer);

    // After drain: reserves are 0, total_shares is preserved, and pool is drained (#1095).
    let info_after = amm.get_info();
    assert_eq!(info_after.reserve_a, 0, "reserve_a not zeroed");
    assert_eq!(info_after.reserve_b, 0, "reserve_b not zeroed");
    assert_eq!(
        info_after.total_shares, info.total_shares,
        "total_shares must remain intact after exec_multisig_emergency_wd (#1095)"
    );
    assert!(amm.is_drained(), "pool must be marked drained");

    // Re-deposit is rejected with PoolDrained.
    let new_provider = Address::generate(&env);
    ta_sac.mint(&new_provider, &2_000_000_i128);
    tb_sac.mint(&new_provider, &1_000_000_i128);
    let res = amm.try_add_liquidity(
        &new_provider,
        &2_000_000_i128,
        &1_000_000_i128,
        &0_i128,
        &u64::MAX,
    );
    assert_eq!(res, Err(Ok(AmmError::PoolDrained)));
}

#[test]
#[should_panic]
fn test_emergency_withdraw_requires_admin_auth() {
    let env = Env::default();
    let amm_addr = env.register_contract(None, AmmPool);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    let recipient = Address::generate(&env);
    amm.emergency_withdraw(&recipient);
}

#[test]
#[should_panic]
fn test_pause_requires_admin_auth() {
    let env = Env::default();
    let amm_addr = env.register_contract(None, AmmPool);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.pause();
}

#[test]
#[should_panic]
fn test_unpause_requires_admin_auth() {
    let env = Env::default();
    let amm_addr = env.register_contract(None, AmmPool);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.unpause();
}

#[test]
fn test_pause_blocks_read_only_functions_remain_available_then_unpause_succeeds() {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta_client, ta_sac) = create_sac(&env, &admin);
    let (tb_client, tb_sac) = create_sac(&env, &admin);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
    );

    let provider = Address::generate(&env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    amm.pause();
    assert!(amm.is_paused());

    let info = amm.get_info();
    assert_eq!(info.reserve_a, 1_000_000);
    assert_eq!(info.reserve_b, 1_000_000);

    let quote = amm.get_amount_out(&ta_client.address, &100_000_i128);
    assert!(quote > 0);
    assert_eq!(amm.shares_of(&provider), shares);

    amm.unpause();
    assert!(!amm.is_paused());

    let trader = Address::generate(&env);
    ta_sac.mint(&trader, &100_000_i128);
    let out = Swap::new(&amm, &trader, &ta_client.address, 100_000).execute();
    assert!(out > 0);

    let extra_provider = Address::generate(&env);
    ta_sac.mint(&extra_provider, &100_000_i128);
    tb_sac.mint(&extra_provider, &100_000_i128);
    let extra_shares = AddLiquidity::new(&amm, &extra_provider, 100_000, 100_000).execute();
    assert!(extra_shares > 0);

    let (out_a, out_b) = RemoveLiquidity::new(&amm, &provider, shares).execute();
    assert!(out_a > 0 && out_b > 0);
}

#[test]
#[should_panic]
fn test_add_liquidity_panics_when_paused() {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta_client, ta_sac) = create_sac(&env, &admin);
    let (tb_client, tb_sac) = create_sac(&env, &admin);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
    );

    let provider = Address::generate(&env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);

    amm.pause();
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();
}

#[test]
#[should_panic]
fn test_swap_panics_when_paused() {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta_client, ta_sac) = create_sac(&env, &admin);
    let (tb_client, tb_sac) = create_sac(&env, &admin);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
    );

    let provider = Address::generate(&env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(&env);
    ta_sac.mint(&trader, &100_000_i128);

    amm.pause();
    Swap::new(&amm, &trader, &ta_client.address, 100_000).execute();
}

#[test]
#[should_panic]
fn test_remove_liquidity_panics_when_paused() {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta_client, ta_sac) = create_sac(&env, &admin);
    let (tb_client, tb_sac) = create_sac(&env, &admin);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
    );

    let provider = Address::generate(&env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    amm.pause();
    RemoveLiquidity::new(&amm, &provider, shares).execute();
}

#[test]
fn test_protocol_fee_accrual() {
    let (env, admin, amm_addr, lp_addr, _) = setup();

    let (ta_client, ta_sac) = create_sac(&env, &admin);
    let (tb_client, tb_sac) = create_sac(&env, &admin);

    let amm = AmmPoolClient::new(&env, &amm_addr);
    // fee_bps=30, protocol_fee_bps=5
    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &5_i128,
    );

    let provider = Address::generate(&env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(&env);
    ta_sac.mint(&trader, &200_000_i128);

    // Two swaps of 100_000 A each — protocol fee per swap = 100_000 * 5 / 10_000 = 50
    Swap::new(&amm, &trader, &ta_client.address, 100_000).execute();
    Swap::new(&amm, &trader, &ta_client.address, 100_000).execute();

    let admin_bal_before = ta_client.balance(&admin);
    let (withdrawn_a, withdrawn_b) = amm.withdraw_protocol_fees();
    let admin_bal_after = ta_client.balance(&admin);

    assert_eq!(withdrawn_a, 100_i128); // 50 + 50
    assert_eq!(withdrawn_b, 0_i128);
    assert_eq!(admin_bal_after - admin_bal_before, 100_i128);
}

#[test]
fn test_withdraw_resets_accrued() {
    let (env, admin, amm_addr, lp_addr, _) = setup();

    let (ta_client, ta_sac) = create_sac(&env, &admin);
    let (tb_client, tb_sac) = create_sac(&env, &admin);

    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &5_i128,
    );

    let provider = Address::generate(&env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(&env);
    ta_sac.mint(&trader, &100_000_i128);
    Swap::new(&amm, &trader, &ta_client.address, 100_000).execute();

    // First withdrawal collects accrued fees.
    let (w1_a, _) = amm.withdraw_protocol_fees();
    assert!(w1_a > 0);

    // Second withdrawal: accrued balances were reset to zero.
    let (w2_a, w2_b) = amm.withdraw_protocol_fees();
    assert_eq!(w2_a, 0_i128);
    assert_eq!(w2_b, 0_i128);
}

#[test]
fn test_reaccrual_after_withdrawal() {
    let (env, admin, amm_addr, lp_addr, _) = setup();

    let (ta_client, ta_sac) = create_sac(&env, &admin);
    let (tb_client, tb_sac) = create_sac(&env, &admin);

    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &5_i128,
    );

    let provider = Address::generate(&env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(&env);
    ta_sac.mint(&trader, &200_000_i128);

    // Swap → withdraw → swap again → withdraw: fees re-accrue after reset.
    Swap::new(&amm, &trader, &ta_client.address, 100_000).execute();
    let (w1, _) = amm.withdraw_protocol_fees();
    assert!(w1 > 0);

    Swap::new(&amm, &trader, &ta_client.address, 100_000).execute();
    let (w2, _) = amm.withdraw_protocol_fees();
    assert!(w2 > 0);
}

// Issue #132: PoolInfo must expose admin, fee_recipient, and protocol_fee_bps.
#[test]
fn test_get_info_returns_admin_and_fee_recipient() {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta, _) = create_sac(&env, &admin);
    let (tb, _) = create_sac(&env, &admin);
    let fee_recipient = Address::generate(&env);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &30_i128,
        &fee_recipient,
        &5_i128,
    );

    let info = amm.get_info();
    assert_eq!(info.admin, admin);
    assert_eq!(info.fee_recipient, fee_recipient);
    assert_eq!(info.protocol_fee_bps, 5_i128);
    assert_eq!(info.fee_bps, 30_i128);
}

// Issue #131: get_accrued_fees must return (0, 0) before swaps.
#[test]
fn test_get_accrued_fees_zero_before_swaps() {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta, _) = create_sac(&env, &admin);
    let (tb, _) = create_sac(&env, &admin);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &30_i128,
        &admin,
        &5_i128,
    );

    let (a, b) = amm.get_accrued_fees();
    assert_eq!(a, 0_i128);
    assert_eq!(b, 0_i128);
}

// Issue #131: get_accrued_fees must match accumulation after swaps without
// mutating state.
#[test]
fn test_get_accrued_fees_matches_swap_accumulation() {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta_client, ta_sac) = create_sac(&env, &admin);
    let (tb_client, tb_sac) = create_sac(&env, &admin);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &5_i128,
    );

    let provider = Address::generate(&env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(&env);
    ta_sac.mint(&trader, &100_000_i128);
    Swap::new(&amm, &trader, &ta_client.address, 100_000).execute();

    // protocol fee per swap = 100_000 * 5 / 10_000 = 50, accrued in token A.
    let (accrued_a, accrued_b) = amm.get_accrued_fees();
    assert_eq!(accrued_a, 50_i128);
    assert_eq!(accrued_b, 0_i128);

    // Calling get_accrued_fees does not mutate state — withdrawing now
    // returns the same amount.
    let (withdrawn_a, withdrawn_b) = amm.withdraw_protocol_fees();
    assert_eq!(withdrawn_a, 50_i128);
    assert_eq!(withdrawn_b, 0_i128);

    // After withdrawal, accrued is back to zero.
    let (post_a, post_b) = amm.get_accrued_fees();
    assert_eq!(post_a, 0_i128);
    assert_eq!(post_b, 0_i128);
}

// Issue #130: propose_admin must emit `admin_nominated`.
#[test]
fn test_propose_admin_emits_event() {
    use soroban_sdk::testutils::Events as _;
    use soroban_sdk::IntoVal;

    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let nominee = Address::generate(env);

    amm.propose_admin(&ts.admin, &nominee);

    let events = env.events().all();
    let evt = events
        .iter()
        .find(|e| e.0 == amm.address && e.1 == (Symbol::new(env, "admin_nominated"),).into_val(env))
        .expect("admin_nominated event not found");

    let __ver_2: (u32, (Address, Address)) = evt.2.into_val(env);
    assert_eq!(__ver_2.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
    let data: (Address, Address) = __ver_2.1;
    assert_eq!(data, (ts.admin.clone(), nominee.clone()));
}

// Issue #130: accept_admin must emit `admin_changed`.
#[test]
fn test_accept_admin_emits_event() {
    use soroban_sdk::testutils::Events as _;
    use soroban_sdk::IntoVal;

    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let nominee = Address::generate(env);

    amm.propose_admin(&ts.admin, &nominee);
    amm.accept_admin(&nominee);

    let events = env.events().all();
    let evt = events
        .iter()
        .find(|e| e.0 == amm.address && e.1 == (Symbol::new(env, "admin_changed"),).into_val(env))
        .expect("admin_changed event not found");

    let __ver_3: (u32, (Address,)) = evt.2.into_val(env);
    assert_eq!(__ver_3.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
    let data: (Address,) = __ver_3.1;
    assert_eq!(data, (nominee,));
}

// Issue #193: simulate_swap price_impact_bps grows for large swaps.
#[test]
fn test_simulate_swap_price_impact_bps() {
    let ts = setup_pool(30); // 0.30 % fee
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);

    let provider = Address::generate(env);
    let ta_sac = soroban_sdk::token::StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = soroban_sdk::token::StellarAssetClient::new(env, &ts.tb_addr);
    ta_sac.mint(&provider, &2_000_000_i128);
    tb_sac.mint(&provider, &2_000_000_i128);

    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    // Tiny swap: price_impact_bps should be 0 (rounds to 0 at 1 unit).
    let tiny = amm.simulate_swap(&ts.ta_addr, &1_i128);
    assert_eq!(tiny.price_impact_bps, 0);

    // Large swap: price_impact_bps must be positive.
    let large = amm.simulate_swap(&ts.ta_addr, &100_000_i128);
    assert!(
        large.price_impact_bps > 0,
        "price_impact_bps must be positive for large swap"
    );

    // Larger swap must have higher price impact than smaller swap.
    let medium = amm.simulate_swap(&ts.ta_addr, &10_000_i128);
    assert!(
        large.price_impact_bps > medium.price_impact_bps,
        "larger swap must have larger price impact"
    );
    assert!(
        medium.price_impact_bps > tiny.price_impact_bps,
        "medium swap must have larger price impact than tiny"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Issue #1097: swap_exact_out fee consistency & k monotonicity
// ─────────────────────────────────────────────────────────────────────────────

/// Acceptance criterion 4:
/// For fees 0, 1, 30, 100, 5_000, 9_999 and amounts spanning 1, 1_000, 99% of reserve:
/// swap_exact_out never charges less than the get_amount_out-consistent price
/// (fee never negative for LPs: k does not decrease).
#[test]
fn test_swap_exact_out_never_undercharges_and_k_never_decreases() {
    let fee_tiers = [0_i128, 1_i128, 30_i128, 100_i128, 5_000_i128, 9_999_i128];
    let reserve: i128 = 1_000_000;
    // Amounts spanning 1, 1_000, and 99% of reserve (990_000)
    let test_amounts = [1_i128, 1_000_i128, (reserve * 99) / 100];

    for &fee_bps in fee_tiers.iter() {
        for &amount_out in test_amounts.iter() {
            let ts = setup_pool(fee_bps);
            let env = &ts.env;
            let amm = AmmPoolClient::new(env, &ts.amm_addr);
            let ta_sac = soroban_sdk::token::StellarAssetClient::new(env, &ts.ta_addr);
            let tb_sac = soroban_sdk::token::StellarAssetClient::new(env, &ts.tb_addr);

            let provider = Address::generate(env);
            ta_sac.mint(&provider, &reserve);
            tb_sac.mint(&provider, &reserve);
            AddLiquidity::new(&amm, &provider, reserve, reserve).execute();

            let initial_k = reserve * reserve;

            // 1. Query required input via get_amount_in
            let amount_in = amm.get_amount_in(&ts.tb_addr, &amount_out);
            assert!(
                amount_in >= 1,
                "amount_in must be >= 1 (fee_bps={}, amount_out={})",
                fee_bps,
                amount_out
            );

            // 2. Consistent price check: if trader deposited amount_in, get_amount_out must yield >= amount_out
            let consistent_out = amm.get_amount_out(&ts.ta_addr, &amount_in);
            assert!(
                consistent_out >= amount_out,
                "swap_exact_out must never charge less than get_amount_out-consistent price: \
                 got consistent_out={}, wanted amount_out={} for fee_bps={}",
                consistent_out,
                amount_out,
                fee_bps
            );

            // 3. Execute swap_exact_out and verify k does not decrease
            let trader = Address::generate(env);
            ta_sac.mint(&trader, &amount_in);

            let charged_in =
                amm.swap_exact_out(&trader, &ts.tb_addr, &amount_out, &amount_in, &u64::MAX);
            assert_eq!(charged_in, amount_in);

            let info = amm.get_info();
            let k_after = info.reserve_a * info.reserve_b;
            assert!(
                k_after >= initial_k,
                "k must not decrease: initial_k={}, k_after={}, fee_bps={}, amount_out={}",
                initial_k,
                k_after,
                fee_bps,
                amount_out
            );
        }
    }
}

proptest! {
    /// Property: For any valid fee in [0, 9_999] and amount_out in [1, 99% of reserve],
    /// required amount_in >= 1, consistent get_amount_out >= amount_out, and k_after >= k_before.
    #[test]
    fn prop_swap_exact_out_fee_and_k_invariants(
        fee_bps in prop_oneof![
            Just(0_i128),
            Just(1_i128),
            Just(30_i128),
            Just(100_i128),
            Just(5_000_i128),
            Just(9_999_i128),
            0_i128..10_000_i128,
        ],
        reserve_in in 100_000_i128..=10_000_000_i128,
        reserve_out in 100_000_i128..=10_000_000_i128,
        pct in 1_i128..=99_i128,
    ) {
        let amount_out = (reserve_out * pct) / 100;
        if amount_out > 0 && amount_out < reserve_out {
            let numerator = reserve_in * amount_out * 10_000;
            let denominator = (reserve_out - amount_out) * (10_000 - fee_bps);
            let amount_in = (numerator / denominator) + 1;
            prop_assert!(amount_in >= 1);

            let amount_in_with_fee = amount_in * (10_000 - fee_bps);
            let consistent_out = amount_in_with_fee * reserve_out / (reserve_in * 10_000 + amount_in_with_fee);
            prop_assert!(consistent_out >= amount_out);

            let k_before = reserve_in * reserve_out;
            let k_after = (reserve_in + amount_in) * (reserve_out - amount_out);
            prop_assert!(k_after >= k_before);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]
    #[test]
    fn prop_test_reserves_balance_invariant_random_operations(
        ops in prop::collection::vec((0u8..=3, 100_i128..=50_000_i128), 5..=15),
    ) {
        let ts = setup_pool(30);
        let env = &ts.env;
        let amm = AmmPoolClient::new(env, &ts.amm_addr);
        let ta_sac = soroban_sdk::token::StellarAssetClient::new(env, &ts.ta_addr);
        let tb_sac = soroban_sdk::token::StellarAssetClient::new(env, &ts.tb_addr);

        amm.set_protocol_fee(&ts.admin, &ts.admin, &10_i128);
        amm.set_lp_rebate(&ts.admin, &200_i128);

        let seed_lp = Address::generate(env);
        ta_sac.mint(&seed_lp, &10_000_000_i128);
        tb_sac.mint(&seed_lp, &10_000_000_i128);
        AddLiquidity::new(&amm, &seed_lp, 10_000_000, 10_000_000).execute();

        let user = Address::generate(env);
        ta_sac.mint(&user, &10_000_000_i128);
        tb_sac.mint(&user, &10_000_000_i128);

        for (action, amount) in ops {
            match action {
                0 => {
                    // add_liquidity
                    AddLiquidity::new(&amm, &user, amount, amount).execute();
                }
                1 => {
                    // swap A for B
                    Swap::new(&amm, &user, &ts.ta_addr, amount).execute();
                }
                2 => {
                    // remove_liquidity_one_sided (token_a)
                    let shares = amm.shares_of(&user);
                    if shares > 10 {
                        let to_burn = (shares / 4).max(1);
                        let _ = amm.remove_liquidity_one_sided(&user, &to_burn, &ts.ta_addr, &0_i128, &u64::MAX);
                    }
                }
                3 => {
                    // remove_liquidity_one_sided (token_b)
                    let shares = amm.shares_of(&user);
                    if shares > 10 {
                        let to_burn = (shares / 4).max(1);
                        let _ = amm.remove_liquidity_one_sided(&user, &to_burn, &ts.tb_addr, &0_i128, &u64::MAX);
                    }
                }
                _ => {}
            }
            assert_reserves_match_balances(&amm, env);
        }
    }
}
