//! Upgrade-path integration tests for governance (#934), concentrated_liquidity
//! (#933), oracle_aggregator (#936) and staking (#935).
//!
//! Each contract is deployed from its release WASM, initialized, and given
//! real state. Every test then:
//!
//! 1. upgrades the contract in place to a *different* WASM (`token.wasm`, the
//!    same swap-in the factory's own upgrade test uses) and proves that code
//!    is now executing at the same address: the token's `admin()` getter —
//!    which none of these contracts export — answers, reading the admin the
//!    original contract's `initialize` wrote, while the original getters are
//!    gone;
//! 2. uses the swapped-in code's own admin-gated `upgrade` to move the address
//!    back onto the original WASM, and asserts every piece of pre-upgrade state
//!    is unchanged and still drives the contract's business logic;
//! 3. asserts a non-admin `upgrade` is rejected with the contract's typed
//!    authorization error, and that a matching admin without authorization is
//!    rejected by the host.
//!
//! The round trip is only possible because `upgrade` replaces bytecode without
//! touching storage: the contract address, instance storage (admin,
//! configuration) and persistent storage (positions, proposals, stakes) all
//! carry across both code swaps.

use soroban_sdk::{
    contract, contractimpl,
    testutils::{Address as _, Ledger},
    token::{Client as TokenClient, StellarAssetClient},
    Address, BytesN, Env, String,
};

use amm::{AmmPool, AmmPoolClient};
use concentrated_liquidity::{ClError, ConcentratedLiquidityClient, WASM as CL_WASM};
use governance::{
    GovernanceClient, GovernanceError, ProposalKind, Vote, VoteRecord, WASM as GOV_WASM,
};
use oracle_aggregator::{
    OracleAggregatorClient, OracleSourceType, PairKey, MAX_PAGE_SIZE, WASM as ORACLE_WASM,
};
use staking::{StakingClient, StakingError, WASM as STAKING_WASM};
use token::{LpToken, LpTokenClient, WASM as TOKEN_WASM};

/// Upgrade the contract at `addr` onto `token.wasm` via `upgrade`, prove the
/// swapped-in code is live and reads the original admin, then use that code's
/// own admin-gated `upgrade` to restore `original_hash`.
///
/// `original_entrypoint_gone` must call a getter of the original contract and
/// report whether it failed; it is checked while the swapped-in code is live.
fn swap_in_new_code_and_restore(
    env: &Env,
    addr: &Address,
    admin: &Address,
    original_hash: &BytesN<32>,
    upgrade: impl FnOnce(&BytesN<32>),
    original_entrypoint_gone: impl FnOnce() -> bool,
) {
    let new_hash: BytesN<32> = env.deployer().upload_contract_wasm(TOKEN_WASM);
    assert_ne!(&new_hash, original_hash, "swap-in WASM must differ");

    upgrade(&new_hash);

    // New code is live at the same address and sees the old instance storage.
    let swapped_in = LpTokenClient::new(env, addr);
    assert_eq!(
        swapped_in.admin(),
        *admin,
        "swapped-in code must read the admin written by the original initialize"
    );
    assert!(
        original_entrypoint_gone(),
        "original entrypoints must be gone while the new WASM is installed"
    );

    // The swapped-in code's own admin-gated upgrade restores the original.
    swapped_in.upgrade(original_hash);
}

// ── #934 governance ──────────────────────────────────────────────────────────

#[test]
fn governance_upgrade_preserves_proposals_votes_and_config() {
    let env = Env::default();
    env.budget().reset_unlimited();
    env.mock_all_auths_allowing_non_root_auth();
    env.ledger().set_timestamp(1_000_000);

    let admin = Address::generate(&env);
    let lp1 = Address::generate(&env);
    let lp2 = Address::generate(&env);

    let lp_addr = env.register_contract(None, LpToken);
    let lp = LpTokenClient::new(&env, &lp_addr);
    lp.initialize(
        &admin,
        &String::from_str(&env, "AMM LP"),
        &String::from_str(&env, "ALP"),
        &7_u32,
    );
    let ta = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let tb = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    // Governance is deployed from its release WASM, exactly as on-chain.
    let gov_hash: BytesN<32> = env.deployer().upload_contract_wasm(GOV_WASM);
    let gov_addr = env.register_contract_wasm(None, GOV_WASM);
    let amm_addr = env.register_contract(None, AmmPool);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(&gov_addr, &ta, &tb, &lp_addr, &30_i128, &admin, &0_i128);

    let gov = GovernanceClient::new(&env, &gov_addr);
    gov.initialize(
        &admin,
        &amm_addr,
        &lp_addr,
        &(7 * 24 * 60 * 60_u64),
        &(2 * 24 * 60 * 60_u64),
        &1_000_i128,
        &100_i128,
    );
    lp.set_locker(&gov_addr);
    lp.mint(&lp1, &600_i128);
    lp.mint(&lp2, &400_i128);

    // Pre-upgrade state: an open proposal with one locked vote.
    let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
    gov.vote(&lp1, &pid, &Vote::For);

    let proposal_before = gov.get_proposal(&pid);
    let params_before = gov.get_params();
    assert_eq!(gov.get_proposal_count(), 1);
    assert_eq!(proposal_before.votes_for, 600);
    assert_eq!(gov.get_vote_info(&pid, &lp1), VoteRecord::VotedFor);
    assert_eq!(lp.locked_balance(&lp1), 600);

    swap_in_new_code_and_restore(
        &env,
        &gov_addr,
        &admin,
        &gov_hash,
        |h| gov.upgrade(&admin, h),
        || gov.try_get_proposal_count().is_err(),
    );

    // Old state survived both code swaps.
    assert_eq!(gov.get_proposal_count(), 1);
    assert_eq!(gov.get_proposal(&pid), proposal_before);
    assert_eq!(gov.get_vote_info(&pid, &lp1), VoteRecord::VotedFor);
    assert_eq!(gov.get_vote_info(&pid, &lp2), VoteRecord::DidNotVote);
    assert_eq!(lp.locked_balance(&lp1), 600);
    let params_after = gov.get_params();
    assert_eq!(
        params_after.voting_period_secs,
        params_before.voting_period_secs
    );
    assert_eq!(params_after.timelock_secs, params_before.timelock_secs);
    assert_eq!(params_after.quorum_bps, params_before.quorum_bps);
    assert_eq!(
        params_after.min_proposer_stake_bps,
        params_before.min_proposer_stake_bps
    );

    // Not re-initialized: initialize still refuses to run.
    assert_eq!(
        gov.try_initialize(&admin, &amm_addr, &lp_addr, &1, &1, &1, &0),
        Err(Ok(GovernanceError::AlreadyInitialized))
    );

    // The preserved proposal completes its lifecycle on the restored code.
    gov.vote(&lp2, &pid, &Vote::For);
    env.ledger()
        .set_timestamp(proposal_before.execute_after + 1);
    gov.execute(&pid);
    assert_eq!(amm.get_info().fee_bps, 50);

    // Unauthorized upgrades are rejected with the typed error.
    let attacker = Address::generate(&env);
    assert_eq!(
        gov.try_upgrade(&attacker, &gov_hash),
        Err(Ok(GovernanceError::Unauthorized))
    );
    env.set_auths(&[]);
    assert!(
        gov.try_upgrade(&admin, &gov_hash).is_err(),
        "upgrade must require the stored admin's authorization"
    );
}

// ── #933 concentrated_liquidity ──────────────────────────────────────────────

#[test]
fn concentrated_liquidity_upgrade_preserves_pool_and_positions() {
    let env = Env::default();
    env.budget().reset_unlimited();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);

    let admin = Address::generate(&env);
    let provider = Address::generate(&env);
    let trader = Address::generate(&env);

    let ta = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let tb = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let (token_a, token_b) = if ta < tb { (ta, tb) } else { (tb, ta) };

    let cl_hash: BytesN<32> = env.deployer().upload_contract_wasm(CL_WASM);
    let cl_addr = env.register_contract_wasm(None, CL_WASM);
    let cl = ConcentratedLiquidityClient::new(&env, &cl_addr);
    let spacing = 60_i32;
    cl.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &spacing);

    StellarAssetClient::new(&env, &token_a).mint(&provider, &10_000_000_i128);
    StellarAssetClient::new(&env, &token_b).mint(&provider, &10_000_000_i128);
    StellarAssetClient::new(&env, &token_a).mint(&trader, &1_000_000_i128);

    // Pre-upgrade state: two positions and a fee-accruing swap.
    let (lo1, hi1) = (-spacing, spacing);
    let (lo2, hi2) = (-4 * spacing, 4 * spacing);
    for (lo, hi) in [(lo1, hi1), (lo2, hi2)] {
        cl.mint_position(
            &provider,
            &lo,
            &hi,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
    }
    // Large enough relative to the pooled liquidity that the fee-growth
    // accumulator (scaled by 1e6 and divided by active liquidity) doesn't
    // truncate to zero.
    let out = cl.swap(&trader, &true, &500_000_i128, &0_u128, &0_i128, &u64::MAX);
    assert!(out > 0);

    let pool_before = cl.get_pool_state();
    let pos1_before = cl.get_position(&provider, &lo1, &hi1);
    let pos2_before = cl.get_position(&provider, &lo2, &hi2);
    let positions_before = cl.get_positions(&provider);
    let tick_before = cl.get_tick_info(&lo1);
    let fees_inside_before = cl.fee_growth_inside(&lo1, &hi1);
    let tokens_before = cl.get_tokens();
    let fee_bps_before = cl.fee_bps();
    let bal_a_before = TokenClient::new(&env, &token_a).balance(&cl_addr);
    let bal_b_before = TokenClient::new(&env, &token_b).balance(&cl_addr);
    assert!(pos1_before.liquidity > 0 && pos2_before.liquidity > 0);
    assert_eq!(positions_before.len(), 2);
    assert!(fees_inside_before.0 > 0, "the swap must have accrued fees");

    swap_in_new_code_and_restore(
        &env,
        &cl_addr,
        &admin,
        &cl_hash,
        |h| cl.upgrade(&admin, h),
        || cl.try_current_tick().is_err(),
    );

    // Pool and position state survived both code swaps.
    assert_eq!(cl.get_pool_state(), pool_before);
    assert_eq!(cl.get_position(&provider, &lo1, &hi1), pos1_before);
    assert_eq!(cl.get_position(&provider, &lo2, &hi2), pos2_before);
    assert_eq!(cl.get_positions(&provider), positions_before);
    assert_eq!(cl.get_tick_info(&lo1), tick_before);
    assert_eq!(cl.fee_growth_inside(&lo1, &hi1), fees_inside_before);
    assert_eq!(cl.get_tokens(), tokens_before);
    assert_eq!(cl.fee_bps(), fee_bps_before);
    assert_eq!(
        TokenClient::new(&env, &token_a).balance(&cl_addr),
        bal_a_before
    );
    assert_eq!(
        TokenClient::new(&env, &token_b).balance(&cl_addr),
        bal_b_before
    );
    assert!(!cl.is_paused());

    // Not re-initialized.
    assert_eq!(
        cl.try_initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &spacing),
        Err(Ok(ClError::AlreadyInitialized))
    );

    // The preserved position still collects its fees and burns on the
    // restored code.
    let (fee_a, _) = cl.collect_fees(&provider, &lo1, &hi1);
    assert!(
        fee_a > 0,
        "fees accrued before the upgrade must be collectable"
    );
    let (ret_a, ret_b) = cl.burn_position(&provider, &lo1, &hi1, &pos1_before.liquidity);
    assert!(ret_a > 0 || ret_b > 0);

    // Unauthorized upgrades are rejected with the typed error.
    let attacker = Address::generate(&env);
    assert_eq!(
        cl.try_upgrade(&attacker, &cl_hash),
        Err(Ok(ClError::Unauthorized))
    );
    env.set_auths(&[]);
    assert!(
        cl.try_upgrade(&admin, &cl_hash).is_err(),
        "upgrade must require the stored admin's authorization"
    );
}

// ── #936 oracle_aggregator ───────────────────────────────────────────────────

/// Minimal price source: `quote` returns a stored price stamped with the
/// current ledger time.
#[contract]
pub struct UpgradeTestOracleSource;

#[contractimpl]
impl UpgradeTestOracleSource {
    pub fn set_price(env: Env, price: i128) {
        env.storage()
            .instance()
            .set(&soroban_sdk::symbol_short!("price"), &price);
    }

    pub fn quote(env: Env, _token_a: Address, _token_b: Address) -> (i128, u64) {
        let price: i128 = env
            .storage()
            .instance()
            .get(&soroban_sdk::symbol_short!("price"))
            .unwrap_or(0);
        (price, env.ledger().timestamp())
    }
}

fn deploy_oracle_source(env: &Env, price: i128) -> Address {
    let id = env.register_contract(None, UpgradeTestOracleSource);
    UpgradeTestOracleSourceClient::new(env, &id).set_price(&price);
    id
}

#[test]
fn oracle_aggregator_upgrade_preserves_sources_and_config() {
    let env = Env::default();
    env.budget().reset_unlimited();
    env.mock_all_auths();
    env.ledger().set_timestamp(10_000);

    let admin = Address::generate(&env);
    let token_a = Address::generate(&env);
    let token_b = Address::generate(&env);
    let pair = PairKey {
        token_a: token_a.clone(),
        token_b: token_b.clone(),
    };

    let oracle_hash: BytesN<32> = env.deployer().upload_contract_wasm(ORACLE_WASM);
    let oracle_addr = env.register_contract_wasm(None, ORACLE_WASM);
    let oracle = OracleAggregatorClient::new(&env, &oracle_addr);
    oracle.initialize(&admin, &600_u64);

    // Pre-upgrade state: three weighted sources and a tuned configuration.
    let s1 = deploy_oracle_source(&env, 1_000_000);
    let s2 = deploy_oracle_source(&env, 1_010_000);
    let s3 = deploy_oracle_source(&env, 1_020_000);
    oracle.register_source(&admin, &pair, &s1, &OracleSourceType::AmmTwap, &10_000);
    oracle.register_source(&admin, &pair, &s2, &OracleSourceType::ClTwap, &20_000);
    oracle.register_source(&admin, &pair, &s3, &OracleSourceType::External, &10_000);
    oracle.set_source_weight(&admin, &pair, &s3, &30_000);
    oracle.set_max_deviation_bps(&admin, &800);

    let price_before = oracle.get_price(&token_a, &token_b);
    let sources_before = oracle.list_sources(&pair, &0, &MAX_PAGE_SIZE);
    assert_eq!(sources_before.len(), 3);
    assert!(price_before.price > 0 && price_before.confidence > 0);

    swap_in_new_code_and_restore(
        &env,
        &oracle_addr,
        &admin,
        &oracle_hash,
        |h| oracle.upgrade(&admin, h),
        || oracle.try_list_sources(&pair, &0, &MAX_PAGE_SIZE).is_err(),
    );

    // Sources and configuration survived both code swaps.
    let sources_after = oracle.list_sources(&pair, &0, &MAX_PAGE_SIZE);
    assert_eq!(sources_after.len(), sources_before.len());
    for (before, after) in sources_before.iter().zip(sources_after.iter()) {
        assert_eq!(after.source_contract, before.source_contract);
        assert_eq!(after.source_type, before.source_type);
        assert_eq!(after.weight, before.weight);
        assert_eq!(after.last_updated_at, before.last_updated_at);
    }
    assert_eq!(oracle.get_admin(), admin);
    assert_eq!(oracle.get_max_staleness(), 600);
    assert_eq!(oracle.get_max_deviation_bps(), 800);
    assert!(!oracle.is_paused());
    let price_after = oracle.get_price(&token_a, &token_b);
    assert_eq!(price_after.price, price_before.price);
    assert_eq!(price_after.confidence, price_before.confidence);

    // Not re-initialized.
    assert!(
        oracle.try_initialize(&admin, &600_u64).is_err(),
        "already-initialized oracle must reject re-initialization"
    );

    // The preserved admin still administers the restored code.
    let s4 = deploy_oracle_source(&env, 1_005_000);
    oracle.register_source(&admin, &pair, &s4, &OracleSourceType::External, &10_000);
    assert_eq!(oracle.list_sources(&pair, &0, &MAX_PAGE_SIZE).len(), 4);

    // Unauthorized upgrades are rejected.
    let attacker = Address::generate(&env);
    assert!(
        oracle.try_upgrade(&attacker, &oracle_hash).is_err(),
        "upgrade must be rejected for a non-admin caller"
    );
    env.set_auths(&[]);
    assert!(
        oracle.try_upgrade(&admin, &oracle_hash).is_err(),
        "upgrade must require the stored admin's authorization"
    );
}

// ── #935 staking ─────────────────────────────────────────────────────────────

#[test]
fn staking_upgrade_preserves_stakes_locks_and_rewards() {
    let env = Env::default();
    env.budget().reset_unlimited();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000_000);

    let admin = Address::generate(&env);
    let alice = Address::generate(&env);
    let bob = Address::generate(&env);

    let lp_token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let reward_token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    StellarAssetClient::new(&env, &lp_token).mint(&alice, &10_000_i128);
    StellarAssetClient::new(&env, &lp_token).mint(&bob, &10_000_i128);
    StellarAssetClient::new(&env, &reward_token).mint(&admin, &1_000_000_i128);

    let staking_hash: BytesN<32> = env.deployer().upload_contract_wasm(STAKING_WASM);
    let staking_addr = env.register_contract_wasm(None, STAKING_WASM);
    let staking = StakingClient::new(&env, &staking_addr);
    staking.initialize(&lp_token, &reward_token, &admin);

    // Pre-upgrade state: a plain stake, a boosted locked stake, and
    // distributed-but-unclaimed rewards.
    let lock_secs = 30 * 24 * 60 * 60_u64;
    staking.add_rewards(&admin, &500_000_i128);
    staking.stake(&alice, &4_000_i128);
    staking.stake_locked(&bob, &6_000_i128, &lock_secs);
    staking.update_rewards(&admin, &100_000_i128);

    let pool_before = staking.get_pool_info();
    let alice_before = staking.get_staker_info(&alice);
    let bob_before = staking.get_staker_info(&bob);
    let alice_pending = staking.pending_rewards(&alice);
    let bob_pending = staking.pending_rewards(&bob);
    assert_eq!(alice_before.staked_amount, 4_000);
    assert_eq!(bob_before.staked_amount, 6_000);
    assert!(bob_before.lock_expiry > env.ledger().timestamp());
    assert!(alice_pending > 0 && bob_pending > 0);

    swap_in_new_code_and_restore(
        &env,
        &staking_addr,
        &admin,
        &staking_hash,
        |h| staking.upgrade(&admin, h),
        || staking.try_get_pool_info().is_err(),
    );

    // Staking state survived both code swaps.
    assert_eq!(staking.get_pool_info(), pool_before);
    for (staker, before) in [(&alice, &alice_before), (&bob, &bob_before)] {
        let after = staking.get_staker_info(staker);
        assert_eq!(after.staked_amount, before.staked_amount);
        assert_eq!(after.effective_amount, before.effective_amount);
        assert_eq!(after.rewards_debt, before.rewards_debt);
        assert_eq!(after.lock_expiry, before.lock_expiry);
        assert_eq!(after.boost_multiplier, before.boost_multiplier);
    }
    assert_eq!(staking.pending_rewards(&alice), alice_pending);
    assert_eq!(staking.pending_rewards(&bob), bob_pending);
    assert_eq!(
        TokenClient::new(&env, &lp_token).balance(&staking_addr),
        10_000
    );

    // Not re-initialized.
    assert_eq!(
        staking.try_initialize(&lp_token, &reward_token, &admin),
        Err(Ok(StakingError::AlreadyInitialized))
    );

    // Rewards accrued before the upgrade are claimable on the restored code,
    // and the lock recorded before the upgrade is still enforced.
    assert_eq!(staking.claim(&alice), alice_pending);
    assert_eq!(
        TokenClient::new(&env, &reward_token).balance(&alice),
        alice_pending
    );
    assert_eq!(
        staking.try_unstake(&bob, &6_000_i128),
        Err(Ok(StakingError::StillLocked))
    );

    // Unauthorized upgrades are rejected with the typed error.
    let attacker = Address::generate(&env);
    assert_eq!(
        staking.try_upgrade(&attacker, &staking_hash),
        Err(Ok(StakingError::Unauthorized))
    );
    env.set_auths(&[]);
    assert!(
        staking.try_upgrade(&admin, &staking_hash).is_err(),
        "upgrade must require the stored admin's authorization"
    );
}
