//! Memory-leak detection: storage entries must not outlive the object they
//! belong to.
//!
//! Issue #827. Soroban has no API to enumerate a contract's storage keys, so
//! these tests do the next best thing: they reconstruct the exact `DataKey` an
//! operation should have written, then assert — via `env.as_contract` on the
//! contract's own storage — that the key is gone once the owning object has
//! been destroyed. That is a real leak assertion, not a "did not panic" smoke
//! check: an implementation that forgot to `remove()` the entry fails here.

use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::StellarAssetClient,
    Address, Env,
};

use amm::{AmmPool, AmmPoolClient};
use concentrated_liquidity::{ConcentratedLiquidity, ConcentratedLiquidityClient};
use governance::{Governance, GovernanceClient, ProposalKind, Vote};
use incentive_campaigns::{IncentiveCampaigns, IncentiveCampaignsClient};
use token::{LpToken, LpTokenClient};

/// A freshly deployed contract owns no storage beyond what `initialize` wrote.
///
/// This is the original `detect_orphaned_storage` scenario, rewritten to assert
/// something falsifiable: each contract is deployed *without* being initialized
/// and must report no state, rather than merely not panicking.
#[test]
fn detect_orphaned_storage() {
    let env = Env::default();
    env.budget().reset_unlimited();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);

    let admin = Address::generate(&env);

    let amm_addr = env.register_contract(None, AmmPool);
    let cl_addr = env.register_contract(None, ConcentratedLiquidity);
    let gov_addr = env.register_contract(None, Governance);
    let incentives_addr = env.register_contract(None, IncentiveCampaigns);

    // An uninitialized CL pool reports no ticks and no positions anywhere in
    // the range it would use, so no tick registry entries were left behind by a
    // previous deployment sharing the address space.
    let cl = ConcentratedLiquidityClient::new(&env, &cl_addr);
    for tick in [-120, -60, 0, 60, 120] {
        assert!(
            !cl.is_tick_initialized(&tick),
            "fresh CL pool must have no initialized tick at {tick}"
        );
    }
    assert!(
        cl.try_get_position(&admin, &-60, &60).is_err(),
        "fresh CL pool must hold no positions"
    );

    // An uninitialized governance contract knows about no proposals.
    let gov = GovernanceClient::new(&env, &gov_addr);
    assert!(
        gov.try_get_proposal(&0).is_err(),
        "fresh governance must hold no proposals"
    );

    // An uninitialized incentives contract has created no campaigns and so has
    // no `ProviderSnapshot` / `DistributionRecord` rows.
    let incentives = IncentiveCampaignsClient::new(&env, &incentives_addr);
    assert_eq!(incentives.get_campaign_count(), 0);
    assert_eq!(incentives.get_distribution_count(&0), 0);

    // The AMM pool likewise reports nothing before initialization.
    let amm = AmmPoolClient::new(&env, &amm_addr);
    assert!(
        amm.try_get_info().is_err(),
        "fresh AMM pool must expose no pool info"
    );
}

/// Fully burning a concentrated-liquidity position must clear its tick-registry
/// entries: both boundary ticks drop out of storage once no position references
/// them, and the range drops out of the provider's `PositionList` index.
#[test]
fn memory_leak_detect_orphaned_storage_after_position_burn() {
    let env = Env::default();
    env.budget().reset_unlimited();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);

    let admin = Address::generate(&env);
    let provider = Address::generate(&env);

    let ta = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let tb = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    // The CL pool orders its pair, so hand it the addresses in sorted order.
    let (token_a, token_b) = if ta < tb { (ta, tb) } else { (tb, ta) };

    let cl_addr = env.register_contract(None, ConcentratedLiquidity);
    let cl = ConcentratedLiquidityClient::new(&env, &cl_addr);
    let tick_spacing = 60_i32;
    cl.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &tick_spacing);

    StellarAssetClient::new(&env, &token_a).mint(&provider, &10_000_000_i128);
    StellarAssetClient::new(&env, &token_b).mint(&provider, &10_000_000_i128);

    let lower = -tick_spacing;
    let upper = tick_spacing;

    // Nothing is registered before the mint.
    assert!(!cl.is_tick_initialized(&lower));
    assert!(!cl.is_tick_initialized(&upper));

    cl.mint_position(
        &provider,
        &lower,
        &upper,
        &1_000_000_i128,
        &1_000_000_i128,
        &0_i128,
        &0_i128,
        &u64::MAX,
    );

    // The mint registered both boundary ticks and the position row.
    assert!(
        cl.is_tick_initialized(&lower),
        "mint must initialize the lower tick"
    );
    assert!(
        cl.is_tick_initialized(&upper),
        "mint must initialize the upper tick"
    );
    let position = cl.get_position(&provider, &lower, &upper);
    assert!(position.liquidity > 0);

    // Burn the entire position.
    cl.burn_position(&provider, &lower, &upper, &position.liquidity);

    // Both tick-registry entries are gone: no orphans.
    assert!(
        !cl.is_tick_initialized(&lower),
        "lower tick entry orphaned after a full burn"
    );
    assert!(
        !cl.is_tick_initialized(&upper),
        "upper tick entry orphaned after a full burn"
    );
    // The position row itself is deliberately retained at zero liquidity — it
    // still carries the fee-growth checkpoint the provider collects against —
    // but it must hold no liquidity and must be dropped from the provider's
    // `PositionList`, which is the index that would otherwise grow unboundedly.
    assert_eq!(
        cl.get_position(&provider, &lower, &upper).liquidity,
        0,
        "a fully burned position must retain no liquidity"
    );
    let list: soroban_sdk::Vec<(i32, i32)> = env.as_contract(&cl_addr, || {
        env.storage()
            .persistent()
            .get(&concentrated_liquidity::DataKey::PositionList(
                provider.clone(),
            ))
            .unwrap_or_else(|| soroban_sdk::Vec::new(&env))
    });
    assert!(
        !list.iter().any(|r| r == (lower, upper)),
        "PositionList entry orphaned after a full burn"
    );
    assert_eq!(
        cl.active_liquidity(),
        0,
        "no liquidity may remain active after the only position is burned"
    );
    // The tick bitmap must not keep pointing at the cleared ticks either.
    assert_eq!(cl.next_initialized_tick_pub(&(lower - tick_spacing)), None);
}

/// A full propose -> vote -> execute -> unlock_vote cycle must leave no
/// `LockedVote` entry behind, and must release the voter's locked LP balance.
///
/// `HasVoted` is deliberately *retained*: it is the double-vote guard for the
/// proposal's whole lifetime, so its presence is correct bookkeeping rather
/// than a leak. This test pins both halves of that contract.
#[test]
fn memory_leak_detect_orphaned_storage_after_governance_proposal_lifecycle() {
    let env = Env::default();
    env.budget().reset_unlimited();
    env.mock_all_auths_allowing_non_root_auth();
    env.ledger().set_timestamp(1_000_000);

    let admin = Address::generate(&env);
    let lp1 = Address::generate(&env);
    let lp2 = Address::generate(&env);

    let lp_addr = env.register_contract(None, LpToken);
    let lp_client = LpTokenClient::new(&env, &lp_addr);
    lp_client.initialize(
        &admin,
        &soroban_sdk::String::from_str(&env, "AMM LP"),
        &soroban_sdk::String::from_str(&env, "ALP"),
        &7_u32,
    );

    let ta = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let tb = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    let gov_addr = env.register_contract(None, Governance);
    let amm_addr = env.register_contract(None, AmmPool);
    AmmPoolClient::new(&env, &amm_addr)
        .initialize(&gov_addr, &ta, &tb, &lp_addr, &30_i128, &admin, &0_i128);

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
    lp_client.set_locker(&gov_addr);

    lp_client.mint(&lp1, &600_i128);
    lp_client.mint(&lp2, &400_i128);

    let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
    gov.vote(&lp1, &pid, &Vote::For);
    gov.vote(&lp2, &pid, &Vote::For);

    // Voting locked LP balances, which is the state that must be released.
    assert_eq!(lp_client.locked_balance(&lp1), 600);
    assert_eq!(lp_client.locked_balance(&lp2), 400);

    let proposal = gov.get_proposal(&pid);
    env.ledger().set_timestamp(proposal.execute_after + 1);
    gov.execute(&pid);

    gov.unlock_vote(&lp1, &pid);
    gov.unlock_vote(&lp2, &pid);

    // No LP balance stays locked once the proposal has concluded and unlocked.
    assert_eq!(
        lp_client.locked_balance(&lp1),
        0,
        "lp1's balance stayed locked after unlock_vote"
    );
    assert_eq!(lp_client.locked_balance(&lp2), 0);

    // And the `LockedVote` rows themselves are removed, not merely zeroed: a
    // second `unlock_vote` finds nothing left to unlock.
    assert!(
        gov.try_unlock_vote(&lp1, &pid).is_err(),
        "LockedVote entry orphaned after unlock_vote"
    );
    assert!(gov.try_unlock_vote(&lp2, &pid).is_err());

    // `HasVoted` is intentionally kept — it still guards against re-voting.
    assert!(
        gov.try_vote(&lp1, &pid, &Vote::Against).is_err(),
        "the HasVoted guard must survive unlock_vote"
    );
}

/// Creating a campaign, claiming against it, and recovering its leftover funds
/// must keep the audit-trail row count bounded and exact.
///
/// The assertion is a numeric count, not "did not panic": exactly one
/// `DistributionRecord` per paying claim, and no extra rows from the
/// snapshot-initialising claim or from the recovery itself.
#[test]
fn memory_leak_detect_orphaned_storage_after_incentive_campaign_recovery() {
    let env = Env::default();
    env.budget().reset_unlimited();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);

    let admin = Address::generate(&env);
    let gov = Address::generate(&env);
    let provider = Address::generate(&env);

    // AMM pool + LP token the campaign is measured against.
    let lp_addr = env.register_contract(None, LpToken);
    let amm_addr = env.register_contract(None, AmmPool);
    LpTokenClient::new(&env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(&env, "LP"),
        &soroban_sdk::String::from_str(&env, "LP"),
        &7_u32,
    );

    let ta = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let tb = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let reward = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(&admin, &ta, &tb, &lp_addr, &30_i128, &admin, &0_i128);

    StellarAssetClient::new(&env, &ta).mint(&provider, &1_000_000_i128);
    StellarAssetClient::new(&env, &tb).mint(&provider, &1_000_000_i128);
    amm.add_liquidity(&provider, &1_000_000, &1_000_000, &0_i128, &u64::MAX);

    StellarAssetClient::new(&env, &reward).mint(&gov, &10_000_000_i128);

    let incentives_addr = env.register_contract(None, IncentiveCampaigns);
    let incentives = IncentiveCampaignsClient::new(&env, &incentives_addr);
    incentives.initialize(&gov);

    // A single campaign, overfunded so there is a leftover to recover.
    let start = 1_000_u64;
    let end = 5_000_u64;
    let rate = 100_i128;
    let funding = (end - start) as i128 * rate * 2;
    let id = incentives.create_campaign(
        &gov, &amm_addr, &lp_addr, &reward, &start, &end, &rate, &funding,
    );

    assert_eq!(incentives.get_campaign_count(), 1);
    assert_eq!(
        incentives.get_distribution_count(&id),
        0,
        "a fresh campaign must have produced no distribution records"
    );

    // A claim without a stake pays nothing and so must not write a
    // `DistributionRecord`. Staking itself writes none either.
    env.ledger().set_timestamp(2_000);
    assert!(incentives.try_claim_rewards(&provider, &id).is_err());
    incentives.stake(&provider, &id, &500_000);
    assert_eq!(
        incentives.get_distribution_count(&id),
        0,
        "staking must not create a distribution record"
    );
    assert_eq!(incentives.get_claim_history(&provider, &0, &100).len(), 0);

    // Two paying claims produce exactly two records — one each, no duplicates.
    env.ledger().set_timestamp(3_000);
    assert!(incentives.claim_rewards(&provider, &id) > 0);
    env.ledger().set_timestamp(4_000);
    assert!(incentives.claim_rewards(&provider, &id) > 0);

    assert_eq!(
        incentives.get_distribution_count(&id),
        2,
        "exactly one distribution record per paying claim"
    );
    assert_eq!(incentives.get_claim_history(&provider, &0, &100).len(), 2);

    // Recovering the leftover winds the campaign down without emitting further
    // distribution records — recovery is not a payout to a provider.
    let treasury = Address::generate(&env);
    env.ledger().set_timestamp(700_000); // past end_time + the 7-day default claim grace
    assert!(incentives.recover_leftover_funds(&gov, &id, &treasury) > 0);

    assert!(!incentives.get_campaign(&id).active);
    assert_eq!(
        incentives.get_distribution_count(&id),
        2,
        "recovery must not add distribution records"
    );
    assert_eq!(
        incentives.get_claim_history(&provider, &0, &100).len(),
        2,
        "recovery must not add claim-history rows"
    );

    // Row growth is bounded by what was actually paid out, and the campaign row
    // itself is retained (it is the audit record, not garbage).
    let campaign = incentives.get_campaign(&id);
    assert!(campaign.total_distributed > 0);
    assert_eq!(incentives.get_campaign_count(), 1);
    let (accrued, _) = incentives.get_campaign_accrual(&id);
    assert!(
        accrued >= campaign.total_distributed,
        "accrual must bound what was distributed"
    );
}
