//! `pol_vesting` upgrades keep live schedules intact (#1136).
//!
//! The vesting contract custodies LP tokens for months or years, so an upgrade
//! must leave its schedules, commitments and admin addresses where they were.

use pol_vesting::{
    PolVestingContract, PolVestingContractClient, VestingError, WASM as POL_VESTING_WASM,
};
use soroban_sdk::{
    testutils::Address as _, testutils::Ledger, token::StellarAssetClient, Address, BytesN, Env,
};

const TOTAL: i128 = 1_000_000;
const END_LEDGER: u32 = 1_000;

struct Setup {
    env: Env,
    vest: Address,
    governance: Address,
    beneficiary: Address,
    lp: Address,
}

/// A vesting contract holding one live schedule that ends at `END_LEDGER`.
fn setup_with_live_schedule() -> (Setup, u32) {
    let env = Env::default();
    env.mock_all_auths();

    let governance = Address::generate(&env);
    let treasury = Address::generate(&env);
    let beneficiary = Address::generate(&env);
    let pool = Address::generate(&env);
    // The pool administers the LP token it issued, as `create_vesting` requires.
    let lp = env
        .register_stellar_asset_contract_v2(pool.clone())
        .address();

    let vest = env.register_contract(None, PolVestingContract);
    let client = PolVestingContractClient::new(&env, &vest);
    client.initialize(&governance, &treasury);
    StellarAssetClient::new(&env, &lp).mint(&vest, &TOTAL);

    let id = client.create_vesting(
        &governance,
        &beneficiary,
        &lp,
        &pool,
        &TOTAL,
        &0,
        &0,
        &END_LEDGER,
    );

    (
        Setup {
            env,
            vest,
            governance,
            beneficiary,
            lp,
        },
        id,
    )
}

#[test]
fn upgrade_keeps_live_schedule_and_its_commitment() {
    let (s, id) = setup_with_live_schedule();
    let client = PolVestingContractClient::new(&s.env, &s.vest);
    let before = client.get_vesting(&s.beneficiary, &id);

    let new_hash: BytesN<32> = s.env.deployer().upload_contract_wasm(POL_VESTING_WASM);
    client.upgrade(&s.governance, &new_hash);

    assert_eq!(client.get_vesting(&s.beneficiary, &id), before);
    assert_eq!(client.committed(&s.lp), TOTAL);
    assert_eq!(client.get_governance(), s.governance);
}

#[test]
fn upgraded_contract_still_releases_the_live_schedule() {
    let (s, id) = setup_with_live_schedule();
    let client = PolVestingContractClient::new(&s.env, &s.vest);
    let new_hash: BytesN<32> = s.env.deployer().upload_contract_wasm(POL_VESTING_WASM);
    client.upgrade(&s.governance, &new_hash);

    s.env.ledger().with_mut(|l| l.sequence_number = END_LEDGER);
    let released = client.release(&s.beneficiary, &id);

    assert_eq!(released, TOTAL);
    assert_eq!(client.committed(&s.lp), 0);
    assert_eq!(
        soroban_sdk::token::TokenClient::new(&s.env, &s.lp).balance(&s.beneficiary),
        TOTAL
    );
}

#[test]
fn upgrade_by_a_non_governance_caller_is_rejected_and_leaves_schedule_alone() {
    let (s, id) = setup_with_live_schedule();
    let client = PolVestingContractClient::new(&s.env, &s.vest);
    let before = client.get_vesting(&s.beneficiary, &id);
    let outsider = Address::generate(&s.env);
    let new_hash: BytesN<32> = s.env.deployer().upload_contract_wasm(POL_VESTING_WASM);

    let err = client
        .try_upgrade(&outsider, &new_hash)
        .unwrap_err()
        .unwrap();

    assert_eq!(err, VestingError::NotGovernance);
    assert_eq!(client.get_vesting(&s.beneficiary, &id), before);
}
