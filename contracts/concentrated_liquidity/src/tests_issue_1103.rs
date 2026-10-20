#![allow(dead_code)]

use crate::{AggregatedPrice, ClError, ConcentratedLiquidity, ConcentratedLiquidityClient};
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::{
    contract, contractimpl, symbol_short, testutils::Address as _, testutils::Events as _,
    testutils::Ledger as _, Address, Env, IntoVal, Symbol, TryFromVal, Val,
};

#[contract]
pub struct MockOracle;

#[contractimpl]
impl MockOracle {
    pub fn set_data(env: Env, price: i128, confidence: u32) {
        env.storage()
            .instance()
            .set(&symbol_short!("price"), &price);
        env.storage()
            .instance()
            .set(&symbol_short!("conf"), &confidence);
    }

    pub fn get_price_safe(env: Env, _token_a: Address, _token_b: Address) -> AggregatedPrice {
        let price = env
            .storage()
            .instance()
            .get(&symbol_short!("price"))
            .unwrap_or(0);
        let confidence = env
            .storage()
            .instance()
            .get(&symbol_short!("conf"))
            .unwrap_or(0);
        AggregatedPrice { price, confidence }
    }
}

struct TestEnv {
    env: Env,
    admin: Address,
    token_a: Address,
    token_b: Address,
    pool_addr: Address,
    lp: Address,
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

    // Provide liquidity
    let lp = Address::generate(&env);
    StellarAssetClient::new(&env, &token_a).mint(&lp, &100_000_000);
    StellarAssetClient::new(&env, &token_b).mint(&lp, &100_000_000);
    client.mint_position(
        &lp,
        &-100,
        &100,
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
        lp,
        client,
    }
}

fn last_versioned_event<T>(env: &Env, pool: &Address, topic: &str) -> (u32, T)
where
    T: TryFromVal<Env, Val>,
{
    let wanted = Symbol::new(env, topic);
    let evt = env
        .events()
        .all()
        .iter()
        .rfind(|e| {
            e.0 == *pool
                && e.1
                    .get(0)
                    .map(|t| Symbol::try_from_val(env, &t) == Ok(wanted.clone()))
                    .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("no event found with topic {topic}"));
    evt.2.into_val(env)
}

#[test]
fn test_oracle_confidence_zero_swap_fails_closed() {
    let te = setup_test();
    let oracle_addr = te.env.register_contract(None, MockOracle);
    let oracle_client = MockOracleClient::new(&te.env, &oracle_addr);
    oracle_client.set_data(&1_000_000, &0); // confidence == 0

    te.client.set_oracle(&te.admin, &Some(oracle_addr));

    let trader = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&trader, &10_000);

    let res = te.client.try_swap(&trader, &true, &100, &0, &0, &u64::MAX);
    assert_eq!(res, Err(Ok(ClError::OracleDeviationExceeded)));
}

#[test]
fn test_oracle_confidence_zero_swap_exact_out_fails_closed() {
    let te = setup_test();
    let oracle_addr = te.env.register_contract(None, MockOracle);
    let oracle_client = MockOracleClient::new(&te.env, &oracle_addr);
    oracle_client.set_data(&1_000_000, &0);

    te.client.set_oracle(&te.admin, &Some(oracle_addr));

    let trader = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&trader, &10_000);

    let res = te
        .client
        .try_swap_exact_out(&trader, &true, &100, &0, &10_000, &u64::MAX);
    assert_eq!(res, Err(Ok(ClError::OracleDeviationExceeded)));
}

#[test]
fn test_oracle_non_positive_price_fails_closed() {
    let te = setup_test();
    let oracle_addr = te.env.register_contract(None, MockOracle);
    let oracle_client = MockOracleClient::new(&te.env, &oracle_addr);
    oracle_client.set_data(&0, &100); // price == 0

    te.client.set_oracle(&te.admin, &Some(oracle_addr));

    let trader = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&trader, &10_000);

    let res = te.client.try_swap(&trader, &true, &100, &0, &0, &u64::MAX);
    assert_eq!(res, Err(Ok(ClError::OracleDeviationExceeded)));
}

#[test]
fn test_oracle_confidence_positive_within_bound_succeeds() {
    let te = setup_test();
    let oracle_addr = te.env.register_contract(None, MockOracle);
    let oracle_client = MockOracleClient::new(&te.env, &oracle_addr);
    oracle_client.set_data(&1_000_000, &100); // price 1:1, confidence 100

    te.client.set_oracle(&te.admin, &Some(oracle_addr));

    let trader = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&trader, &10_000);

    let out = te.client.swap(&trader, &true, &100, &0, &0, &u64::MAX);
    assert!(out > 0);
}

#[test]
fn test_oracle_confidence_positive_within_bound_swap_exact_out_succeeds() {
    let te = setup_test();
    let oracle_addr = te.env.register_contract(None, MockOracle);
    let oracle_client = MockOracleClient::new(&te.env, &oracle_addr);
    oracle_client.set_data(&1_000_000, &100);

    te.client.set_oracle(&te.admin, &Some(oracle_addr));

    let trader = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&trader, &10_000);

    let inp = te
        .client
        .swap_exact_out(&trader, &true, &50, &0, &10_000, &u64::MAX);
    assert!(inp > 0);
}

#[test]
fn test_oracle_unset_succeeds() {
    let te = setup_test();
    let trader = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&trader, &10_000);

    let out = te.client.swap(&trader, &true, &100, &0, &0, &u64::MAX);
    assert!(out > 0);
}

#[test]
fn test_admin_set_oracle_emits_event() {
    let te = setup_test();
    let oracle_addr = te.env.register_contract(None, MockOracle);

    te.client.set_oracle(&te.admin, &Some(oracle_addr.clone()));
    let (ver, (old_oracle, new_oracle)): (u32, (Option<Address>, Option<Address>)) =
        last_versioned_event(&te.env, &te.pool_addr, "oracle_set");
    assert_eq!(ver, 1);
    assert_eq!(old_oracle, None);
    assert_eq!(new_oracle, Some(oracle_addr));
}

#[test]
fn test_admin_set_position_nft_emits_event() {
    let te = setup_test();
    let nft_addr = Address::generate(&te.env);

    te.client
        .set_position_nft(&te.admin, &Some(nft_addr.clone()));
    let (ver, (old_nft, new_nft)): (u32, (Option<Address>, Option<Address>)) =
        last_versioned_event(&te.env, &te.pool_addr, "position_nft_set");
    assert_eq!(ver, 1);
    assert_eq!(old_nft, None);
    assert_eq!(new_nft, Some(nft_addr));
}

#[test]
fn test_admin_set_max_oracle_deviation_bps_emits_event() {
    let te = setup_test();

    te.client.set_max_oracle_deviation_bps(&te.admin, &300);
    let (ver, (old_bps, new_bps)): (u32, (i128, i128)) =
        last_versioned_event(&te.env, &te.pool_addr, "max_oracle_deviation_set");
    assert_eq!(ver, 1);
    assert_eq!(old_bps, 500);
    assert_eq!(new_bps, 300);
}

#[test]
fn test_admin_pause_and_unpause_emit_events() {
    let te = setup_test();

    te.client.pause(&te.admin);
    let (ver, (admin,)): (u32, (Address,)) = last_versioned_event(&te.env, &te.pool_addr, "pause");
    assert_eq!(ver, 1);
    assert_eq!(admin, te.admin);

    te.client.unpause(&te.admin);
    let (ver, (admin,)): (u32, (Address,)) =
        last_versioned_event(&te.env, &te.pool_addr, "unpause");
    assert_eq!(ver, 1);
    assert_eq!(admin, te.admin);
}

#[test]
fn test_admin_nominate_and_accept_emit_events() {
    let te = setup_test();
    let new_admin = Address::generate(&te.env);

    te.client.propose_admin(&te.admin, &new_admin);
    let (ver, (cur_admin, nom_admin)): (u32, (Address, Address)) =
        last_versioned_event(&te.env, &te.pool_addr, "admin_nominated");
    assert_eq!(ver, 1);
    assert_eq!(cur_admin, te.admin);
    assert_eq!(nom_admin, new_admin);

    te.client.accept_admin(&new_admin);
    let (ver, (acc_admin,)): (u32, (Address,)) =
        last_versioned_event(&te.env, &te.pool_addr, "admin_changed");
    assert_eq!(ver, 1);
    assert_eq!(acc_admin, new_admin);
}

#[test]
fn test_admin_set_protocol_fee_strict_bounds_and_event() {
    let te = setup_test();
    let recipient = Address::generate(&te.env);

    // Reject >= 10_000
    let err_10000 = te
        .client
        .try_set_protocol_fee(&te.admin, &recipient, &10_000);
    assert_eq!(err_10000, Err(Ok(ClError::InvalidFeeBps)));

    let err_10001 = te
        .client
        .try_set_protocol_fee(&te.admin, &recipient, &10_001);
    assert_eq!(err_10001, Err(Ok(ClError::InvalidFeeBps)));

    // 0 succeeds
    assert!(te
        .client
        .try_set_protocol_fee(&te.admin, &recipient, &0)
        .is_ok());

    // 9_999 succeeds
    assert!(te
        .client
        .try_set_protocol_fee(&te.admin, &recipient, &9_999)
        .is_ok());

    let (ver, (bps, rec)): (u32, (i128, Address)) =
        last_versioned_event(&te.env, &te.pool_addr, "protocol_fee_set");
    assert_eq!(ver, 1);
    assert_eq!(bps, 9_999);
    assert_eq!(rec, recipient);
}

#[test]
fn test_protocol_fee_lp_receives_positive_share_at_9999() {
    let te = setup_test();
    let recipient = Address::generate(&te.env);
    te.client.set_protocol_fee(&te.admin, &recipient, &9_999);

    let trader = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&trader, &100_000_000);
    te.client
        .swap(&trader, &true, &10_000_000, &0, &0, &u64::MAX);

    let fees_lp = te.client.collect_fees(&te.lp, &-100, &100);
    // At 9,999 bps, protocol gets 99.99% of fees, but LP still gets > 0
    assert!(fees_lp.0 > 0 || fees_lp.1 > 0);
}

#[test]
fn test_admin_withdraw_protocol_fees_emits_event() {
    let te = setup_test();
    let recipient = Address::generate(&te.env);
    te.client.set_protocol_fee(&te.admin, &recipient, &5_000);

    let trader = Address::generate(&te.env);
    StellarAssetClient::new(&te.env, &te.token_a).mint(&trader, &100_000);
    te.client.swap(&trader, &true, &10_000, &0, &0, &u64::MAX);

    te.client.withdraw_protocol_fees(&te.admin);
    let (ver, (rec, fa, fb)): (u32, (Address, i128, i128)) =
        last_versioned_event(&te.env, &te.pool_addr, "protocol_fees_withdrawn");
    assert_eq!(ver, 1);
    assert_eq!(rec, recipient);
    assert!(fa > 0 || fb > 0);
}

#[cfg(feature = "testutils")]
#[test]
fn test_upgrade_extends_instance_ttl() {
    use soroban_sdk::testutils::storage::Instance as _;
    let te = setup_test();
    te.env.budget().reset_unlimited();
    // Lower instance TTL
    te.env
        .ledger()
        .with_mut(|l| l.sequence_number += 3_110_400 - 518_400 + 10);
    let ttl_before = te
        .env
        .as_contract(&te.pool_addr, || te.env.storage().instance().get_ttl());
    assert!(ttl_before < 518_400);

    let new_hash = te.env.deployer().upload_contract_wasm(crate::WASM);
    te.client.upgrade(&te.admin, &new_hash);

    let ttl_after = te
        .env
        .as_contract(&te.pool_addr, || te.env.storage().instance().get_ttl());
    assert!(ttl_after > ttl_before);
    assert!(ttl_after >= 3_110_400 - 1);
}
