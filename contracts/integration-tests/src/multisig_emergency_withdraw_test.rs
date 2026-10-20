//! Integration tests for the AMM k-of-n multisig emergency withdrawal flow.
//!
//! Issue #391: covers set_multisig, propose_emergency_withdraw,
//! exec_multisig_emergency_wd, and the unilateral emergency_withdraw guard.

use amm::{AmmError, AmmPool, AmmPoolClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger, LedgerInfo},
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env, Vec,
};
use token::LpToken;

const DEADLINE: u64 = u64::MAX;

fn set_ledger_ts(env: &Env, ts: u64) {
    env.ledger().set(LedgerInfo {
        timestamp: ts,
        protocol_version: 22,
        sequence_number: env.ledger().sequence(),
        network_id: Default::default(),
        base_reserve: 10,
        min_temp_entry_ttl: 16,
        min_persistent_entry_ttl: 4096,
        max_entry_ttl: 6_312_000,
    });
}

struct Suite {
    env: Env,
    amm_addr: Address,
    token_a: Address,
    token_b: Address,
    admin: Address,
}

fn setup_suite() -> Suite {
    let env = Env::default();
    env.budget().reset_unlimited();
    env.mock_all_auths();
    set_ledger_ts(&env, 1_000);

    let admin = Address::generate(&env);
    let token_a = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_b = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    // Register the pool first so the LP token can be administered by it. The LP
    // token's admin must be the pool contract (not an EOA) so the pool is
    // authorized to mint/burn LP tokens — including the minimum-liquidity lock
    // minted to the pool's own address on the first deposit. Using an EOA here
    // trips a non-root-authorization panic under `mock_all_auths()`.
    let amm_addr = env.register_contract(None, AmmPool);

    let lp_addr = env.register_contract(None, LpToken);
    token::LpTokenClient::new(&env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(&env, "AMM LP"),
        &soroban_sdk::String::from_str(&env, "ALP"),
        &7u32,
    );

    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin, &token_a, &token_b, &lp_addr, &30_i128, &admin, &0_i128,
    );

    // Seed the pool with reserves.
    let provider = Address::generate(&env);
    StellarAssetClient::new(&env, &token_a).mint(&provider, &1_000_000_i128);
    StellarAssetClient::new(&env, &token_b).mint(&provider, &1_000_000_i128);
    amm.add_liquidity(&provider, &500_000_i128, &500_000_i128, &1_i128, &DEADLINE);

    Suite {
        env,
        amm_addr,
        token_a,
        token_b,
        admin,
    }
}

fn signer_vec(env: &Env, signers: &[Address]) -> Vec<Address> {
    let mut v = Vec::new(env);
    for s in signers {
        v.push_back(s.clone());
    }
    v
}

#[test]
fn scenario_multisig_non_signer_proposal_fails() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let c = Address::generate(&s.env);
    let non_signer = Address::generate(&s.env);
    let recipient = Address::generate(&s.env);

    let amm = AmmPoolClient::new(&s.env, &s.amm_addr);
    amm.set_multisig(
        &s.admin,
        &signer_vec(&s.env, &[a.clone(), b.clone(), c.clone()]),
        &2u32,
    );

    let result = amm.try_propose_emergency_withdraw(&non_signer, &recipient);
    assert!(result.is_err(), "non-signer proposal should fail");
}

#[test]
fn scenario_multisig_single_approval_does_not_execute() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let c = Address::generate(&s.env);
    let recipient = Address::generate(&s.env);

    let amm = AmmPoolClient::new(&s.env, &s.amm_addr);
    amm.set_multisig(
        &s.admin,
        &signer_vec(&s.env, &[a.clone(), b.clone(), c.clone()]),
        &2u32,
    );

    amm.propose_emergency_withdraw(&a, &recipient);

    // Only one approval collected; execution must fail with QuorumNotMet before quorum is reached.
    let result = amm.try_exec_multisig_emergency_wd(&a);
    assert_eq!(
        result,
        Err(Ok(AmmError::QuorumNotMet)),
        "execution with 1/2 approvals should fail with QuorumNotMet"
    );
}

#[test]
fn scenario_multisig_exec_without_proposal_returns_no_pending_proposal() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);

    let amm = AmmPoolClient::new(&s.env, &s.amm_addr);
    amm.set_multisig(
        &s.admin,
        &signer_vec(&s.env, &[a.clone(), b.clone()]),
        &2u32,
    );

    let result = amm.try_exec_multisig_emergency_wd(&a);
    assert_eq!(
        result,
        Err(Ok(AmmError::NoPendingProposal)),
        "executing without a pending proposal must return NoPendingProposal"
    );
}

#[test]
fn scenario_multisig_quorum_triggers_emergency_withdrawal() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let c = Address::generate(&s.env);
    let recipient = Address::generate(&s.env);

    let amm = AmmPoolClient::new(&s.env, &s.amm_addr);
    amm.set_multisig(
        &s.admin,
        &signer_vec(&s.env, &[a.clone(), b.clone(), c.clone()]),
        &2u32,
    );

    let pool_a_before = TokenClient::new(&s.env, &s.token_a).balance(&s.amm_addr);
    let pool_b_before = TokenClient::new(&s.env, &s.token_b).balance(&s.amm_addr);
    let recipient_a_before = TokenClient::new(&s.env, &s.token_a).balance(&recipient);
    let recipient_b_before = TokenClient::new(&s.env, &s.token_b).balance(&recipient);
    let shares_before = amm.get_info().total_shares;

    // Collect quorum (2 approvals) before executing.
    amm.propose_emergency_withdraw(&a, &recipient);
    amm.propose_emergency_withdraw(&b, &recipient);
    amm.exec_multisig_emergency_wd(&b);

    assert!(amm.is_drained(), "pool should enter terminal Drained state");
    assert_eq!(
        amm.get_info().total_shares,
        shares_before,
        "total_shares must remain intact after multisig drain"
    );

    // Pool reserves should have been drained to the recipient.
    assert_eq!(
        TokenClient::new(&s.env, &s.token_a).balance(&s.amm_addr),
        0,
        "reserve_a should be drained"
    );
    assert_eq!(
        TokenClient::new(&s.env, &s.token_b).balance(&s.amm_addr),
        0,
        "reserve_b should be drained"
    );
    assert_eq!(
        TokenClient::new(&s.env, &s.token_a).balance(&recipient),
        recipient_a_before + pool_a_before,
        "recipient should receive token_a reserves"
    );
    assert_eq!(
        TokenClient::new(&s.env, &s.token_b).balance(&recipient),
        recipient_b_before + pool_b_before,
        "recipient should receive token_b reserves"
    );
}

#[test]
fn scenario_multisig_re_execution_returns_already_executed() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let recipient = Address::generate(&s.env);

    let amm = AmmPoolClient::new(&s.env, &s.amm_addr);
    amm.set_multisig(
        &s.admin,
        &signer_vec(&s.env, &[a.clone(), b.clone()]),
        &2u32,
    );

    // Collect quorum (2 approvals) so the first execution succeeds.
    amm.propose_emergency_withdraw(&a, &recipient);
    amm.propose_emergency_withdraw(&b, &recipient);
    amm.exec_multisig_emergency_wd(&b);

    // Re-executing the same proposal must fail with AlreadyExecuted.
    let result = amm.try_exec_multisig_emergency_wd(&a);
    assert_eq!(
        result,
        Err(Ok(AmmError::AlreadyExecuted)),
        "second execution should return AlreadyExecuted"
    );
}

#[test]
fn scenario_multisig_expired_proposal_cannot_execute() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let recipient = Address::generate(&s.env);

    let amm = AmmPoolClient::new(&s.env, &s.amm_addr);
    amm.set_multisig(
        &s.admin,
        &signer_vec(&s.env, &[a.clone(), b.clone()]),
        &2u32,
    );

    amm.propose_emergency_withdraw(&a, &recipient);

    // Advance past the 7-day proposal TTL.
    set_ledger_ts(&s.env, 1_000 + (7 * 24 * 60 * 60) + 1);

    let result = amm.try_exec_multisig_emergency_wd(&b);
    assert_eq!(
        result,
        Err(Ok(AmmError::ProposalExpired)),
        "expired proposal should return ProposalExpired"
    );
}

#[test]
fn scenario_plain_emergency_withdraw_blocked_with_multisig() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let recipient = Address::generate(&s.env);

    let amm = AmmPoolClient::new(&s.env, &s.amm_addr);
    amm.set_multisig(
        &s.admin,
        &signer_vec(&s.env, &[a.clone(), b.clone()]),
        &2u32,
    );

    // Even the admin must use the multisig flow when a quorum is configured.
    let result = amm.try_emergency_withdraw(&recipient);
    assert_eq!(
        result,
        Err(Ok(AmmError::Unauthorized)),
        "plain emergency_withdraw should be blocked while multisig guard is active"
    );
}
