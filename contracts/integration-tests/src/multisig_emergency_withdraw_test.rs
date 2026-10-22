//! Integration tests for the AMM k-of-n multisig emergency withdrawal flow.
//!
//! Issue #391: covers set_multisig, propose_emergency_withdraw,
//! approve_multisig_proposal, exec_multisig_emergency_wd, and the unilateral
//! emergency_withdraw guard.
//!
//! Updated for the guard-hardening issue: an active multisig guard cannot be
//! changed or disabled by the admin it guards, emergency withdrawals require a
//! pause and an announced delay, and distinct error variants are returned.

use amm::{AmmError, AmmPool, AmmPoolClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger, LedgerInfo},
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env, Vec,
};
use token::LpToken;

const DEADLINE: u64 = u64::MAX;
const EMERGENCY_DELAY_SECS: u64 = 24 * 60 * 60;
const T0: u64 = 1_000;

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
    lp_addr: Address,
    token_a: Address,
    token_b: Address,
    admin: Address,
}

impl Suite {
    fn amm(&self) -> AmmPoolClient<'_> {
        AmmPoolClient::new(&self.env, &self.amm_addr)
    }
}

/// Register a pool and its LP token, initialized with `emergency_delay_secs`,
/// but with no liquidity.
fn register_pool(
    env: &Env,
    emergency_delay_secs: u64,
) -> (Address, Address, Address, Address, Address) {
    let admin = Address::generate(env);
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
    token::LpTokenClient::new(env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(env, "AMM LP"),
        &soroban_sdk::String::from_str(env, "ALP"),
        &7u32,
    );

    let amm = AmmPoolClient::new(env, &amm_addr);
    amm.initialize_with_emergency_delay(
        &admin,
        &token_a,
        &token_b,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
        &emergency_delay_secs,
    );
    (amm_addr, lp_addr, token_a, token_b, admin)
}

fn setup_suite() -> Suite {
    let env = Env::default();
    env.budget().reset_unlimited();
    env.mock_all_auths();
    set_ledger_ts(&env, T0);

    let (amm_addr, lp_addr, token_a, token_b, admin) = register_pool(&env, EMERGENCY_DELAY_SECS);
    let amm = AmmPoolClient::new(&env, &amm_addr);

    // Seed the pool with reserves.
    let provider = Address::generate(&env);
    StellarAssetClient::new(&env, &token_a).mint(&provider, &1_000_000_i128);
    StellarAssetClient::new(&env, &token_b).mint(&provider, &1_000_000_i128);
    amm.add_liquidity(&provider, &500_000_i128, &500_000_i128, &1_i128, &DEADLINE);

    Suite {
        env,
        amm_addr,
        lp_addr,
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

/// Install a multisig guard with `signers` and `quorum`.
fn install_multisig(s: &Suite, signers: &[Address], quorum: u32) {
    s.amm()
        .set_multisig(&s.admin, &signer_vec(&s.env, signers), &quorum);
}

#[test]
fn scenario_multisig_non_signer_proposal_fails() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let c = Address::generate(&s.env);
    let non_signer = Address::generate(&s.env);
    let recipient = Address::generate(&s.env);

    install_multisig(&s, &[a, b, c], 2);

    let result = s
        .amm()
        .try_propose_emergency_withdraw(&non_signer, &recipient);
    assert_eq!(
        result,
        Err(Ok(AmmError::Unauthorized)),
        "non-signer proposal should fail"
    );
}

#[test]
fn scenario_multisig_single_approval_does_not_execute() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let c = Address::generate(&s.env);
    let recipient = Address::generate(&s.env);

    install_multisig(&s, &[a.clone(), b, c], 2);
    let amm = s.amm();

    let pid = amm.propose_emergency_withdraw(&a, &recipient);

    // Only one approval collected; execution must fail with QuorumNotReached.
    let result = amm.try_exec_multisig_emergency_wd(&a, &pid);
    assert_eq!(
        result,
        Err(Ok(AmmError::QuorumNotReached)),
        "execution with 1/2 approvals should fail with QuorumNotReached"
    );
}

#[test]
fn scenario_multisig_exec_without_proposal_returns_no_pending_proposal() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);

    install_multisig(&s, &[a.clone(), b], 2);

    let result = s.amm().try_exec_multisig_emergency_wd(&a, &1_u64);
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

    install_multisig(&s, &[a.clone(), b.clone(), c], 2);
    let amm = s.amm();

    let pool_a_before = TokenClient::new(&s.env, &s.token_a).balance(&s.amm_addr);
    let pool_b_before = TokenClient::new(&s.env, &s.token_b).balance(&s.amm_addr);
    let recipient_a_before = TokenClient::new(&s.env, &s.token_a).balance(&recipient);
    let recipient_b_before = TokenClient::new(&s.env, &s.token_b).balance(&recipient);
    let shares_before = amm.get_info().total_shares;

    // Collect quorum (2 approvals) before executing.
    let pid = amm.propose_emergency_withdraw(&a, &recipient);
    amm.approve_multisig_proposal(&b, &pid);
    amm.pause();

    // The multisig path is also delayed from the moment quorum is reached.
    let early = amm.try_exec_multisig_emergency_wd(&b, &pid);
    assert_eq!(
        early,
        Err(Ok(AmmError::EmergencyDelayNotElapsed)),
        "execution before the delay must fail"
    );
    set_ledger_ts(&s.env, T0 + EMERGENCY_DELAY_SECS + 1);
    amm.exec_multisig_emergency_wd(&b, &pid);

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
fn scenario_multisig_requires_pause() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let recipient = Address::generate(&s.env);

    install_multisig(&s, &[a.clone(), b.clone()], 2);
    let amm = s.amm();

    let pid = amm.propose_emergency_withdraw(&a, &recipient);
    amm.approve_multisig_proposal(&b, &pid);
    set_ledger_ts(&s.env, T0 + EMERGENCY_DELAY_SECS + 1);

    let result = amm.try_exec_multisig_emergency_wd(&b, &pid);
    assert_eq!(
        result,
        Err(Ok(AmmError::Paused)),
        "the multisig path must also require a paused pool"
    );
}

#[test]
fn scenario_multisig_re_execution_returns_already_executed() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let recipient = Address::generate(&s.env);

    install_multisig(&s, &[a.clone(), b.clone()], 2);
    let amm = s.amm();

    // Collect quorum (2 approvals) so the first execution succeeds.
    let pid = amm.propose_emergency_withdraw(&a, &recipient);
    amm.approve_multisig_proposal(&b, &pid);
    amm.pause();
    set_ledger_ts(&s.env, T0 + EMERGENCY_DELAY_SECS + 1);
    amm.exec_multisig_emergency_wd(&b, &pid);

    // Re-executing the same proposal must fail with AlreadyExecuted.
    let result = amm.try_exec_multisig_emergency_wd(&a, &pid);
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

    install_multisig(&s, &[a.clone(), b.clone()], 2);
    let amm = s.amm();

    let pid = amm.propose_emergency_withdraw(&a, &recipient);

    // Advance past the 7-day proposal TTL.
    set_ledger_ts(&s.env, T0 + (7 * 24 * 60 * 60) + 1);

    let result = amm.try_exec_multisig_emergency_wd(&b, &pid);
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

    let amm = s.amm();
    amm.announce_emergency_withdraw(&s.admin, &recipient);
    amm.pause();
    install_multisig(&s, &[a, b], 2);
    set_ledger_ts(&s.env, T0 + EMERGENCY_DELAY_SECS + 1);

    // Even a fully announced, delayed, paused withdrawal by the admin must use
    // the multisig flow once a quorum is configured.
    let result = amm.try_emergency_withdraw(&s.admin, &recipient);
    assert_eq!(
        result,
        Err(Ok(AmmError::Unauthorized)),
        "plain emergency_withdraw should be blocked while multisig guard is active"
    );
}

#[test]
fn scenario_admin_cannot_disable_multisig_alone() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let c = Address::generate(&s.env);

    install_multisig(&s, &[a, b, c], 2);
    let amm = s.amm();

    // A single admin key must not be able to turn the guard off...
    let result = amm.try_set_multisig(&s.admin, &signer_vec(&s.env, &[]), &0u32);
    assert_eq!(
        result,
        Err(Ok(AmmError::MultisigGuardActive)),
        "admin alone must not disable the multisig guard"
    );

    // ...or swap in a signer set it controls.
    let own_signer = Address::generate(&s.env);
    let result = amm.try_set_multisig(&s.admin, &signer_vec(&s.env, &[own_signer]), &1u32);
    assert_eq!(result, Err(Ok(AmmError::MultisigGuardActive)));
}

#[test]
fn scenario_multisig_can_disable_itself() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);

    install_multisig(&s, &[a.clone(), b.clone()], 2);
    let amm = s.amm();

    let pid = amm.propose_set_multisig(&a, &signer_vec(&s.env, &[]), &0u32);

    // One of two approvals is not enough.
    assert_eq!(
        amm.try_exec_multisig_set_multisig(&a, &pid),
        Err(Ok(AmmError::QuorumNotReached))
    );

    amm.approve_multisig_proposal(&b, &pid);
    amm.exec_multisig_set_multisig(&a, &pid);

    let (signers, quorum) = amm.get_multisig();
    assert_eq!(quorum, 0);
    assert_eq!(signers.len(), 0);
}

#[test]
fn scenario_multisig_reconfiguration_invalidates_old_proposals() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let recipient = Address::generate(&s.env);

    install_multisig(&s, &[a.clone(), b.clone()], 2);
    let amm = s.amm();

    // A drain is proposed under the 2-of-2 configuration...
    let drain = amm.propose_emergency_withdraw(&a, &recipient);

    // ...then the signers move to a 1-of-1 set.
    let new_set = Address::generate(&s.env);
    let reconfig = amm.propose_set_multisig(&a, &signer_vec(&s.env, &[new_set]), &1u32);
    amm.approve_multisig_proposal(&b, &reconfig);
    amm.exec_multisig_set_multisig(&a, &reconfig);

    // The stale proposal cannot be approved or executed any more.
    let new_signer = amm.get_multisig().0.get(0).unwrap();
    assert_eq!(
        amm.try_approve_multisig_proposal(&new_signer, &drain),
        Err(Ok(AmmError::ProposalExpired))
    );
}

#[test]
fn scenario_set_multisig_invalid_quorum() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let c = Address::generate(&s.env);

    let result = s
        .amm()
        .try_set_multisig(&s.admin, &signer_vec(&s.env, &[a, b, c]), &4u32);
    assert_eq!(
        result,
        Err(Ok(AmmError::InvalidQuorum)),
        "quorum greater than signer count must return InvalidQuorum"
    );
}

#[test]
fn scenario_set_multisig_rejects_duplicate_signers() {
    let s = setup_suite();
    let a = Address::generate(&s.env);

    let result = s
        .amm()
        .try_set_multisig(&s.admin, &signer_vec(&s.env, &[a.clone(), a]), &2u32);
    assert_eq!(
        result,
        Err(Ok(AmmError::InvalidQuorum)),
        "duplicate signers could make the quorum unreachable"
    );
}

#[test]
fn scenario_emergency_withdraw_requires_pause() {
    let s = setup_suite();
    let recipient = Address::generate(&s.env);

    let amm = s.amm();
    amm.announce_emergency_withdraw(&s.admin, &recipient);
    set_ledger_ts(&s.env, T0 + EMERGENCY_DELAY_SECS + 1);

    let unpaused = amm.try_emergency_withdraw(&s.admin, &recipient);
    assert_eq!(
        unpaused,
        Err(Ok(AmmError::Paused)),
        "emergency_withdraw must require the pool to be paused"
    );
}

#[test]
fn scenario_emergency_withdraw_requires_delay() {
    let s = setup_suite();
    let recipient = Address::generate(&s.env);

    let amm = s.amm();
    amm.announce_emergency_withdraw(&s.admin, &recipient);
    amm.pause();

    let early = amm.try_emergency_withdraw(&s.admin, &recipient);
    assert_eq!(
        early,
        Err(Ok(AmmError::EmergencyDelayNotElapsed)),
        "emergency_withdraw before the delay must fail"
    );
}

#[test]
fn scenario_emergency_withdraw_after_delay_succeeds() {
    let s = setup_suite();
    let recipient = Address::generate(&s.env);

    let amm = s.amm();
    let pool_a_before = TokenClient::new(&s.env, &s.token_a).balance(&s.amm_addr);
    let pool_b_before = TokenClient::new(&s.env, &s.token_b).balance(&s.amm_addr);

    amm.announce_emergency_withdraw(&s.admin, &recipient);
    amm.pause();
    set_ledger_ts(&s.env, T0 + EMERGENCY_DELAY_SECS + 1);
    amm.emergency_withdraw(&s.admin, &recipient);

    assert_eq!(
        TokenClient::new(&s.env, &s.token_a).balance(&recipient),
        pool_a_before,
        "recipient should receive token_a reserves"
    );
    assert_eq!(
        TokenClient::new(&s.env, &s.token_b).balance(&recipient),
        pool_b_before,
        "recipient should receive token_b reserves"
    );
}

#[test]
fn scenario_cancel_emergency_withdraw_resets_announcement() {
    let s = setup_suite();
    let recipient = Address::generate(&s.env);

    let amm = s.amm();
    amm.announce_emergency_withdraw(&s.admin, &recipient);
    amm.cancel_emergency_withdraw(&s.admin);
    amm.pause();
    set_ledger_ts(&s.env, T0 + EMERGENCY_DELAY_SECS + 1);

    let result = amm.try_emergency_withdraw(&s.admin, &recipient);
    assert_eq!(
        result,
        Err(Ok(AmmError::NoPendingProposal)),
        "cancelled announcement must not allow withdrawal"
    );
}

#[test]
fn scenario_reannounce_restarts_delay() {
    let s = setup_suite();
    let recipient = Address::generate(&s.env);

    let amm = s.amm();
    amm.announce_emergency_withdraw(&s.admin, &recipient);
    amm.pause();
    set_ledger_ts(&s.env, T0 + EMERGENCY_DELAY_SECS / 2);
    amm.announce_emergency_withdraw(&s.admin, &recipient);
    // Past the first announcement's delay, but not the second's.
    set_ledger_ts(&s.env, T0 + EMERGENCY_DELAY_SECS + 1);

    let result = amm.try_emergency_withdraw(&s.admin, &recipient);
    assert_eq!(
        result,
        Err(Ok(AmmError::EmergencyDelayNotElapsed)),
        "re-announcing must restart the delay"
    );
}

#[test]
fn scenario_emergency_withdraw_rejects_pool_address() {
    let s = setup_suite();
    let result = s
        .amm()
        .try_announce_emergency_withdraw(&s.admin, &s.amm_addr);
    assert_eq!(
        result,
        Err(Ok(AmmError::InvalidRecipient)),
        "withdrawing to the pool itself must be rejected"
    );
}

#[test]
fn scenario_emergency_withdraw_rejects_lp_token() {
    let s = setup_suite();
    let result = s
        .amm()
        .try_announce_emergency_withdraw(&s.admin, &s.lp_addr);
    assert_eq!(
        result,
        Err(Ok(AmmError::InvalidRecipient)),
        "withdrawing to the LP token must be rejected"
    );
}

#[test]
fn scenario_multisig_emergency_withdraw_rejects_pool_address() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);

    install_multisig(&s, &[a.clone(), b], 2);

    let result = s.amm().try_propose_emergency_withdraw(&a, &s.amm_addr);
    assert_eq!(
        result,
        Err(Ok(AmmError::InvalidRecipient)),
        "multisig proposal to the pool itself must be rejected"
    );
}

#[test]
fn scenario_multisig_emergency_withdraw_rejects_lp_token() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);

    install_multisig(&s, &[a.clone(), b], 2);

    let result = s.amm().try_propose_emergency_withdraw(&a, &s.lp_addr);
    assert_eq!(
        result,
        Err(Ok(AmmError::InvalidRecipient)),
        "multisig proposal to the LP token must be rejected"
    );
}

#[test]
fn scenario_emergency_delay_cap_enforced() {
    let env = Env::default();
    env.mock_all_auths();
    set_ledger_ts(&env, T0);

    let admin = Address::generate(&env);
    let token_a = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_b = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let amm_addr = env.register_contract(None, AmmPool);
    let lp_addr = env.register_contract(None, LpToken);
    token::LpTokenClient::new(&env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(&env, "AMM LP"),
        &soroban_sdk::String::from_str(&env, "ALP"),
        &7u32,
    );

    let too_long = 7 * 24 * 60 * 60 + 1;
    let result = AmmPoolClient::new(&env, &amm_addr).try_initialize_with_emergency_delay(
        &admin, &token_a, &token_b, &lp_addr, &30_i128, &admin, &0_i128, &too_long,
    );
    assert_eq!(
        result,
        Err(Ok(AmmError::InvalidEmergencyDelay)),
        "delay above the 7-day cap must be rejected"
    );
}

#[test]
fn scenario_multisig_quorum_reached_then_delay() {
    let s = setup_suite();
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let recipient = Address::generate(&s.env);

    install_multisig(&s, &[a.clone(), b.clone()], 2);
    let amm = s.amm();
    amm.pause();

    let pid = amm.propose_emergency_withdraw(&a, &recipient);
    set_ledger_ts(&s.env, T0 + EMERGENCY_DELAY_SECS / 2);
    amm.approve_multisig_proposal(&b, &pid);

    // Past a full delay since the proposal, but not since quorum was reached.
    set_ledger_ts(&s.env, T0 + EMERGENCY_DELAY_SECS + 1);
    let early = amm.try_exec_multisig_emergency_wd(&b, &pid);
    assert_eq!(
        early,
        Err(Ok(AmmError::EmergencyDelayNotElapsed)),
        "delay is measured from quorum being reached"
    );

    set_ledger_ts(
        &s.env,
        T0 + EMERGENCY_DELAY_SECS / 2 + EMERGENCY_DELAY_SECS + 1,
    );
    amm.exec_multisig_emergency_wd(&b, &pid);
    assert!(amm.is_drained(), "pool should be drained after the delay");
}
