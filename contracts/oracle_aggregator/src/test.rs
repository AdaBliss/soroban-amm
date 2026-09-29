extern crate std;

use soroban_sdk::{
    testutils::{Address as _, Events, Ledger, LedgerInfo},
    Address, Env, IntoVal,
};

use super::*;

// ── Mock adapter ────────────────────────────────────────────────────────────
//
// A configurable adapter contract used as the registered source in
// every test. `quote()` reads a per-instance `price` from storage so
// individual tests can dial each source independently.

mod mock_adapter {
    use soroban_sdk::{contract, contractimpl, Address, Env};

    #[contract]
    pub struct MockAdapter;

    #[contractimpl]
    impl MockAdapter {
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
            // Report the current ledger time so a configured (price > 0) source is fresh.
            (price, env.ledger().timestamp())
        }
    }
}
use mock_adapter::{MockAdapter, MockAdapterClient};

struct Harness<'a> {
    aggregator: OracleAggregatorClient<'a>,
    admin: Address,
    token_a: Address,
    token_b: Address,
}

fn deploy(env: &Env, max_staleness: u64) -> Harness<'_> {
    env.mock_all_auths();
    let admin = Address::generate(env);
    let aggregator_id = env.register_contract(None, OracleAggregator);
    let aggregator = OracleAggregatorClient::new(env, &aggregator_id);
    aggregator.initialize(&admin, &max_staleness);
    let token_a = Address::generate(env);
    let token_b = Address::generate(env);
    Harness {
        aggregator,
        admin,
        token_a,
        token_b,
    }
}

fn deploy_source(env: &Env, price: i128) -> Address {
    let id = env.register_contract(None, MockAdapter);
    let client = MockAdapterClient::new(env, &id);
    client.set_price(&price);
    id
}

mod panicking_mock_adapter {
    use soroban_sdk::{contract, contractimpl, Address, Env};

    #[contract]
    pub struct PanickingMockAdapter;

    #[contractimpl]
    impl PanickingMockAdapter {
        pub fn quote(_env: Env, _token_a: Address, _token_b: Address) -> (i128, u64) {
            panic!("simulated source panic");
        }
    }
}
use panicking_mock_adapter::PanickingMockAdapter;

fn deploy_panicking_source(env: &Env) -> Address {
    env.register_contract(None, PanickingMockAdapter)
}

fn set_now(env: &Env, ts: u64) {
    env.ledger().set(LedgerInfo {
        timestamp: ts,
        protocol_version: 22,
        sequence_number: 0,
        network_id: Default::default(),
        base_reserve: 10,
        min_temp_entry_ttl: 1,
        min_persistent_entry_ttl: 1,
        max_entry_ttl: 6_312_000,
    });
}

// ── Existing tests (updated for weight parameter and summed confidence) ─────

#[test]
fn initialize_seeds_admin_and_staleness() {
    let env = Env::default();
    let h = deploy(&env, 600);
    assert_eq!(h.aggregator.get_admin(), h.admin);
    assert_eq!(h.aggregator.get_max_staleness(), 600);
    assert_eq!(h.aggregator.list_sources().len(), 0);
}

#[test]
#[should_panic(expected = "Error(Contract, #1)")]
fn initialize_is_one_time() {
    let env = Env::default();
    let h = deploy(&env, 600);
    h.aggregator.initialize(&h.admin, &600);
}

#[test]
fn register_source_appends_to_registry() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 110);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::External, &10_000);
    let sources = h.aggregator.list_sources();
    assert_eq!(sources.len(), 2);
    assert_eq!(sources.get_unchecked(0).source_contract, s1);
    assert_eq!(sources.get_unchecked(0).weight, 10_000);
    assert_eq!(sources.get_unchecked(1).source_contract, s2);
    assert_eq!(sources.get_unchecked(1).weight, 10_000);
}

#[test]
#[should_panic(expected = "Error(Contract, #4)")]
fn register_source_rejects_duplicates() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
}

#[test]
fn remove_source_drops_the_entry() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 110);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::External, &10_000);
    h.aggregator.remove_source(&h.admin, &s1);
    let sources = h.aggregator.list_sources();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources.get_unchecked(0).source_contract, s2);
}

#[test]
#[should_panic(expected = "Error(Contract, #5)")]
fn remove_source_panics_on_unknown_address() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let ghost = Address::generate(&env);
    h.aggregator.remove_source(&h.admin, &ghost);
}

#[test]
fn get_price_returns_median_of_three_sources() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 110);
    let s3 = deploy_source(&env, 150);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s3, &OracleSourceType::External, &10_000);
    // These prices span 50%; widen the band so this stays a pure median test.
    h.aggregator.set_max_deviation_bps(&h.admin, &5_000);
    set_now(&env, 1_000);

    let result = h.aggregator.get_price(&h.token_a, &h.token_b);
    assert_eq!(result.price, 110);
    // Confidence is summed weight: 3 × 10_000 = 30_000.
    assert_eq!(result.confidence, 30_000);
}

#[test]
fn get_price_returns_two_way_median_average() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 200);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::External, &10_000);
    // 100 vs 200 straddle the median by 33%; widen the band so both count.
    h.aggregator.set_max_deviation_bps(&h.admin, &5_000);
    set_now(&env, 1_000);
    let result = h.aggregator.get_price(&h.token_a, &h.token_b);
    assert_eq!(result.price, 150);
    assert_eq!(result.confidence, 20_000);
}

#[test]
fn get_price_two_way_median_does_not_overflow_on_large_prices() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let hi = i128::MAX;
    let lo = i128::MAX - 2;
    let s1 = deploy_source(&env, hi);
    let s2 = deploy_source(&env, lo);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::External, &10_000);
    set_now(&env, 1_000);
    let result = h.aggregator.get_price(&h.token_a, &h.token_b);
    assert_eq!(result.price, i128::MAX - 1);
    assert_eq!(result.confidence, 20_000);
}

#[test]
fn stale_source_excluded_after_window() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 110);
    let s3 = deploy_source(&env, 150);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s3, &OracleSourceType::External, &10_000);
    h.aggregator.set_max_deviation_bps(&h.admin, &5_000);

    set_now(&env, 1_000);
    h.aggregator.get_price(&h.token_a, &h.token_b);

    let s2_client = MockAdapterClient::new(&env, &s2);
    s2_client.set_price(&0);

    set_now(&env, 1_000 + 200);

    let result = h.aggregator.get_price(&h.token_a, &h.token_b);
    assert_eq!(result.price, 125);
    assert_eq!(result.confidence, 20_000);
}

#[test]
#[should_panic(expected = "Error(Contract, #6)")]
fn single_source_below_min_panics() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    set_now(&env, 1_000);
    h.aggregator.get_price(&h.token_a, &h.token_b);
}

#[test]
#[should_panic(expected = "Error(Contract, #6)")]
fn empty_registry_panics() {
    let env = Env::default();
    let h = deploy(&env, 600);
    set_now(&env, 1_000);
    h.aggregator.get_price(&h.token_a, &h.token_b);
}

#[test]
fn set_max_staleness_updates_window() {
    let env = Env::default();
    let h = deploy(&env, 600);
    h.aggregator.set_max_staleness(&h.admin, &120);
    assert_eq!(h.aggregator.get_max_staleness(), 120);
}

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn set_max_staleness_rejects_zero() {
    let env = Env::default();
    let h = deploy(&env, 600);
    h.aggregator.set_max_staleness(&h.admin, &0);
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn register_source_rejects_non_admin() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let attacker = Address::generate(&env);
    let s1 = deploy_source(&env, 100);
    h.aggregator
        .register_source(&attacker, &s1, &OracleSourceType::AmmTwap, &10_000);
}

#[test]
fn stale_src_event_emitted_when_sources_skipped() {
    let env = Env::default();
    let h = deploy(&env, 60);

    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 0);
    let s3 = deploy_source(&env, 0);

    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s3, &OracleSourceType::External, &10_000);

    set_now(&env, 1_000);

    h.aggregator.get_price_safe(&h.token_a, &h.token_b);

    let events = env.events().all();
    let agg_id = h.aggregator.address.clone();
    let expected_topics: soroban_sdk::Vec<soroban_sdk::Val> =
        (symbol_short!("stale_src"),).into_val(&env);

    let stale_event = events
        .iter()
        .find(|e| e.0 == agg_id && e.1 == expected_topics)
        .expect("stale_src event must be emitted when sources are skipped");

    let (version, (stale_addrs,)): (u32, (soroban_sdk::Vec<Address>,)) =
        stale_event.2.into_val(&env);
    assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
    assert_eq!(stale_addrs.len(), 2);
    assert!(stale_addrs.contains(&s2));
    assert!(stale_addrs.contains(&s3));
}

// ── Deviation band (#466) ────────────────────────────────────────────────────

#[test]
fn get_max_deviation_bps_defaults_to_constant() {
    let env = Env::default();
    let h = deploy(&env, 600);
    assert_eq!(
        h.aggregator.get_max_deviation_bps(),
        DEFAULT_MAX_DEVIATION_BPS
    );
}

#[test]
fn set_max_deviation_bps_updates_band() {
    let env = Env::default();
    let h = deploy(&env, 600);
    h.aggregator.set_max_deviation_bps(&h.admin, &250);
    assert_eq!(h.aggregator.get_max_deviation_bps(), 250);
}

#[test]
#[should_panic(expected = "Error(Contract, #8)")]
fn set_max_deviation_bps_rejects_zero() {
    let env = Env::default();
    let h = deploy(&env, 600);
    h.aggregator.set_max_deviation_bps(&h.admin, &0);
}

#[test]
#[should_panic(expected = "Error(Contract, #8)")]
fn set_max_deviation_bps_rejects_above_max() {
    let env = Env::default();
    let h = deploy(&env, 600);
    h.aggregator.set_max_deviation_bps(&h.admin, &10_001);
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn set_max_deviation_bps_rejects_non_admin() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let attacker = Address::generate(&env);
    h.aggregator.set_max_deviation_bps(&attacker, &250);
}

#[test]
fn deviating_source_excluded_from_confidence_and_price() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 102);
    let s3 = deploy_source(&env, 300);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s3, &OracleSourceType::External, &10_000);
    set_now(&env, 1_000);

    let result = h.aggregator.get_price(&h.token_a, &h.token_b);
    assert_eq!(result.price, 101);
    assert_eq!(result.confidence, 20_000);
}

#[test]
fn two_disagreeing_sources_report_no_confidence() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let honest = deploy_source(&env, 100);
    let attacker = deploy_source(&env, 200);
    h.aggregator
        .register_source(&h.admin, &honest, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &attacker, &OracleSourceType::External, &10_000);
    set_now(&env, 1_000);

    let safe = h.aggregator.get_price_safe(&h.token_a, &h.token_b);
    assert_eq!(safe.confidence, 0);
    assert_eq!(safe.price, 0);
}

#[test]
#[should_panic(expected = "Error(Contract, #6)")]
fn two_disagreeing_sources_panic_on_get_price() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let honest = deploy_source(&env, 100);
    let attacker = deploy_source(&env, 200);
    h.aggregator
        .register_source(&h.admin, &honest, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &attacker, &OracleSourceType::External, &10_000);
    set_now(&env, 1_000);
    h.aggregator.get_price(&h.token_a, &h.token_b);
}

#[test]
fn widening_band_admits_previously_deviant_source() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 102);
    let s3 = deploy_source(&env, 150);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s3, &OracleSourceType::External, &10_000);

    set_now(&env, 1_000);
    let tight = h.aggregator.get_price(&h.token_a, &h.token_b);
    assert_eq!(tight.price, 101);
    assert_eq!(tight.confidence, 20_000);

    h.aggregator.set_max_deviation_bps(&h.admin, &5_000);
    let wide = h.aggregator.get_price(&h.token_a, &h.token_b);
    assert_eq!(wide.price, 102);
    assert_eq!(wide.confidence, 30_000);
}

#[test]
fn deviant_event_lists_out_of_band_sources() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 102);
    let s3 = deploy_source(&env, 300);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s3, &OracleSourceType::External, &10_000);
    set_now(&env, 1_000);

    h.aggregator.get_price_safe(&h.token_a, &h.token_b);

    let events = env.events().all();
    let agg_id = h.aggregator.address.clone();
    let expected_topics: soroban_sdk::Vec<soroban_sdk::Val> =
        (symbol_short!("deviant"),).into_val(&env);
    let deviant_event = events
        .iter()
        .find(|e| e.0 == agg_id && e.1 == expected_topics)
        .expect("deviant event must be emitted when a source is out of band");

    let (version, (deviant_addrs,)): (u32, (soroban_sdk::Vec<Address>,)) =
        deviant_event.2.into_val(&env);
    assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
    assert_eq!(deviant_addrs.len(), 1);
    assert!(deviant_addrs.contains(&s3));
}

#[test]
fn panicking_source_is_skipped_and_does_not_abort_aggregation() {
    let env = Env::default();
    let h = deploy(&env, 600);

    let honest1 = deploy_source(&env, 100);
    let honest2 = deploy_source(&env, 102);
    let panicker = deploy_panicking_source(&env);

    h.aggregator
        .register_source(&h.admin, &honest1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &honest2, &OracleSourceType::ClTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &panicker, &OracleSourceType::External, &10_000);

    set_now(&env, 1_000);

    let result = h.aggregator.get_price(&h.token_a, &h.token_b);
    assert_eq!(result.price, 101);
    assert_eq!(result.confidence, 20_000);
}

// ── Weighted-median tests (#689) ────────────────────────────────────────────

/// With all weights equal, the weighted median must return the same price
/// as the unweighted implementation.
#[test]
fn equal_weight_weighted_matches_unweighted() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 110);
    let s3 = deploy_source(&env, 150);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s3, &OracleSourceType::External, &10_000);
    h.aggregator.set_max_deviation_bps(&h.admin, &5_000);
    set_now(&env, 1_000);

    let result = h.aggregator.get_price(&h.token_a, &h.token_b);
    // Unweighted median of [100, 110, 150] = 110.
    // Weighted median with all weights 10_000: sorted [(100,10k), (110,10k), (150,10k)].
    // Total = 30k, half = 15k. 100 → acc=10k <=15k. 110 → acc=20k > 15k → 110. ✓
    assert_eq!(result.price, 110);
    assert_eq!(result.confidence, 30_000);
}

/// A source with 3× weight moves the weighted median toward its quote.
/// Fixture: prices [100, 110, 120], weights [10_000, 10_000, 30_000].
/// Equal-weight median = 110. Weighted median: total=50k, half=25k.
/// 100→10k, 110→20k, 120→50k > 25k → 120.
#[test]
fn heavy_source_moves_result_toward_its_quote() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 110);
    let s3 = deploy_source(&env, 120);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s3, &OracleSourceType::External, &30_000);
    h.aggregator.set_max_deviation_bps(&h.admin, &5_000);
    set_now(&env, 1_000);

    let result = h.aggregator.get_price(&h.token_a, &h.token_b);
    // Weighted median = 120, pulled toward the heavy source.
    assert_eq!(result.price, 120);
    assert_eq!(result.confidence, 50_000);
}

#[test]
#[should_panic(expected = "Error(Contract, #9)")]
fn register_source_rejects_zero_weight() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &0);
}

#[test]
#[should_panic(expected = "Error(Contract, #9)")]
fn register_source_rejects_weight_above_ceiling() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    h.aggregator.register_source(
        &h.admin,
        &s1,
        &OracleSourceType::AmmTwap,
        &(MAX_SOURCE_WEIGHT + 1),
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #9)")]
fn set_source_weight_rejects_zero() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator.set_source_weight(&h.admin, &s1, &0);
}

#[test]
#[should_panic(expected = "Error(Contract, #9)")]
fn set_source_weight_rejects_above_ceiling() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .set_source_weight(&h.admin, &s1, &(MAX_SOURCE_WEIGHT + 1));
}

/// When agreeing sources sum to less than MIN_AGREEING_WEIGHT, return {0, 0}.
#[test]
fn insufficient_agreeing_weight_returns_zero() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 102);
    // Each source has weight 5_000; total = 10_000 < MIN_AGREEING_WEIGHT (20_000).
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &5_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::External, &5_000);
    set_now(&env, 1_000);

    let result = h.aggregator.get_price_safe(&h.token_a, &h.token_b);
    assert_eq!(result.price, 0);
    assert_eq!(result.confidence, 0);
}

/// A heavy outlier is still classified deviant because the reference median
/// used for the deviation band remains unweighted.
#[test]
fn heavy_outlier_still_deviants_despite_weight() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 102);
    // Heavy source at 300 — even with 5× weight, reference median is unweighted.
    let s3 = deploy_source(&env, 300);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s3, &OracleSourceType::External, &50_000);
    set_now(&env, 1_000);

    let result = h.aggregator.get_price(&h.token_a, &h.token_b);
    // Unweighted reference median = 102. 300 is ~194% above → deviant.
    // Agreeing: (100, 10_000) and (102, 10_000). Weighted median = 101.
    assert_eq!(result.price, 101);
    assert_eq!(result.confidence, 20_000);
}

/// get_price_detailed accounts for every registered source exactly once.
#[test]
fn get_price_detailed_accounts_all_sources() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 102);
    let s3 = deploy_source(&env, 300);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s3, &OracleSourceType::External, &10_000);
    set_now(&env, 1_000);

    let (price, source_quotes) = h.aggregator.get_price_detailed(&h.token_a, &h.token_b);

    // All 3 sources accounted for.
    assert_eq!(source_quotes.len(), 3);
    assert_eq!(price.price, 101);
    assert_eq!(price.confidence, 20_000);

    // Exactly two Agreed, one Deviant.
    let mut agreed_count: u32 = 0;
    let mut deviant_count: u32 = 0;
    for i in 0..source_quotes.len() {
        let sq = source_quotes.get_unchecked(i);
        match sq.status {
            QuoteStatus::Agreed => agreed_count += 1,
            QuoteStatus::Deviant => {
                deviant_count += 1;
                assert_eq!(sq.source, s3);
            }
            _ => {}
        }
    }
    assert_eq!(agreed_count, 2);
    assert_eq!(deviant_count, 1);
}

/// The Agreed subset of get_price_detailed reproduces get_price_safe.
#[test]
fn get_price_detailed_agreed_matches_get_price_safe() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 102);
    let s3 = deploy_source(&env, 150);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s3, &OracleSourceType::External, &10_000);
    set_now(&env, 1_000);

    let safe = h.aggregator.get_price_safe(&h.token_a, &h.token_b);
    let (detailed_price, _) = h.aggregator.get_price_detailed(&h.token_a, &h.token_b);

    assert_eq!(safe.price, detailed_price.price);
    assert_eq!(safe.confidence, detailed_price.confidence);
}

/// get_price_spread_bps computes the spread between highest and lowest
/// agreeing quote.
#[test]
fn get_price_spread_bps_computes_correctly() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 110);
    let s3 = deploy_source(&env, 300); // deviant
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s3, &OracleSourceType::External, &10_000);
    // Widen the band so 100 and 110 agree (100 is ~9% from the reference
    // median 110, needs > 500 bps to pass the default band).
    h.aggregator.set_max_deviation_bps(&h.admin, &5_000);
    set_now(&env, 1_000);

    let spread = h.aggregator.get_price_spread_bps(&h.token_a, &h.token_b);
    // Agreeing: 100, 110. Spread = (110-100)*10_000/100 = 1_000 bps = 10%.
    assert_eq!(spread, 1_000);
}

/// Pre-existing source records without a weight (weight == 0) are backfilled
/// to DEFAULT_SOURCE_WEIGHT by migrate_sources.
#[test]
fn migration_backfills_default_weight() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 110);

    // Register sources with explicit weight.
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap, &20_000);

    // Simulate a pre-upgrade record with weight=0 by manually overwriting
    // the stored sources vector via env.as_contract.
    let mut sources = h.aggregator.list_sources();
    let mut src1 = sources.get_unchecked(0);
    src1.weight = 0;
    sources.set(0, src1);
    let agg_id = h.aggregator.address.clone();
    env.as_contract(&agg_id, || {
        env.storage().instance().set(&DataKey::Sources, &sources);
    });

    // migrate_sources should backfill weight=0 → DEFAULT_SOURCE_WEIGHT.
    h.aggregator.migrate_sources(&h.admin);

    let migrated = h.aggregator.list_sources();
    assert_eq!(migrated.get_unchecked(0).weight, DEFAULT_SOURCE_WEIGHT);
    // s2's weight should be unchanged.
    assert_eq!(migrated.get_unchecked(1).weight, 20_000);
}

// ── Issue #916: every oracle_aggregator event carries EVENT_SCHEMA_VERSION ──
//
// `get_price`, `set_source_weight`, and `aggregate_price`'s `stale_src` /
// `deviant` events used to call `env.events().publish(...)` directly, so
// their payloads were not version-stamped and an indexer reading
// `(version, ...rest)` would have decoded the first real field as the
// version number. These tests pin the stamp in place for all four.

/// Fetch the payload of the most recent event this contract published under
/// `topic`, decoded as a version-stamped `(u32, T)` pair.
fn last_versioned_event<T>(env: &Env, contract_id: &Address, topic: &str) -> (u32, T)
where
    T: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>,
{
    let wanted: soroban_sdk::Vec<soroban_sdk::Val> =
        (soroban_sdk::Symbol::new(env, topic),).into_val(env);
    let evt = env
        .events()
        .all()
        .iter()
        .rfind(|e| &e.0 == contract_id && e.1 == wanted)
        .unwrap_or_else(|| panic!("no `{topic}` event found"));
    evt.2.into_val(env)
}

/// set_source_weight emits a version-stamped src_wt event with old and new
/// weight.
#[test]
fn set_source_weight_emits_versioned_event() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);

    h.aggregator.set_source_weight(&h.admin, &s1, &25_000);

    let (version, (src_addr, old_w, new_w)): (u32, (Address, u32, u32)) =
        last_versioned_event(&env, &h.aggregator.address, "src_wt");
    assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
    assert_eq!(src_addr, s1);
    assert_eq!(old_w, 10_000);
    assert_eq!(new_w, 25_000);
}

/// get_price emits a version-stamped price event with the aggregated price
/// and confidence.
#[test]
fn get_price_emits_versioned_event() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 102);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap, &10_000);
    set_now(&env, 1_000);

    let result = h.aggregator.get_price(&h.token_a, &h.token_b);

    let (version, (token_a, token_b, price, confidence)): (u32, (Address, Address, i128, u32)) =
        last_versioned_event(&env, &h.aggregator.address, "price");
    assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
    assert_eq!(token_a, h.token_a);
    assert_eq!(token_b, h.token_b);
    assert_eq!(price, result.price);
    assert_eq!(confidence, result.confidence);
}

/// A stale (never-quoting) source triggers a version-stamped stale_src event.
#[test]
fn stale_source_emits_versioned_event() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let stale = deploy_source(&env, 0); // never reports a positive price
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &stale, &OracleSourceType::External, &10_000);
    set_now(&env, 1_000);

    h.aggregator.get_price_safe(&h.token_a, &h.token_b);

    let (version, (stale_sources,)): (u32, (soroban_sdk::Vec<Address>,)) =
        last_versioned_event(&env, &h.aggregator.address, "stale_src");
    assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
    assert!(stale_sources.contains(&stale));
}

/// A source outside the deviation band triggers a version-stamped deviant
/// event.
#[test]
fn deviant_source_emits_versioned_event() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let s1 = deploy_source(&env, 100);
    let s2 = deploy_source(&env, 102);
    let outlier = deploy_source(&env, 300);
    h.aggregator
        .register_source(&h.admin, &s1, &OracleSourceType::AmmTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &s2, &OracleSourceType::ClTwap, &10_000);
    h.aggregator
        .register_source(&h.admin, &outlier, &OracleSourceType::External, &10_000);
    set_now(&env, 1_000);

    h.aggregator.get_price_safe(&h.token_a, &h.token_b);

    let (version, (deviant_sources,)): (u32, (soroban_sdk::Vec<Address>,)) =
        last_versioned_event(&env, &h.aggregator.address, "deviant");
    assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
    assert!(deviant_sources.contains(&outlier));
}

// ── #907: instance TTL is extended on every entrypoint ───────────────────────

/// Advancing the ledger far past the default instance-entry TTL and then
/// calling entrypoints must keep the contract responsive rather than trapping
/// on an archived instance entry.
///
/// The oracle aggregator holds all of its state (admin, staleness, sources,
/// deviation band) in instance storage, so if that entry lapses every call
/// traps. Each entrypoint now bumps the instance TTL; this test walks the
/// ledger forward in steps smaller than the bump window, calling in between,
/// and asserts the reads still return. Without the extension the first call
/// after the advance would trap.
#[test]
fn instance_ttl_is_extended_on_access_across_ledger_advance() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|li| {
        li.sequence_number = 1_000;
        li.max_entry_ttl = 6_312_000;
    });

    let aggregator_id = env.register_contract(None, OracleAggregator);
    let aggregator = OracleAggregatorClient::new(&env, &aggregator_id);
    let admin = Address::generate(&env);
    aggregator.initialize(&admin, &3_600);

    // Register a source so the read paths have state to walk as well.
    let source = deploy_source(&env, 1_000);
    aggregator.register_source(&admin, &source, &OracleSourceType::External, &10_000);

    // Step forward in increments smaller than the TTL bump (518_400), calling
    // an entrypoint each step. Each call re-bumps, so the next step stays live.
    for _ in 0..4 {
        env.ledger().with_mut(|li| li.sequence_number += 400_000);
        assert_eq!(aggregator.get_admin(), admin);
        assert_eq!(aggregator.list_sources().len(), 1);
        assert_eq!(aggregator.get_max_staleness(), 3_600);
    }
}



#[test]
fn test_admin_rotation_happy_path() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let new_admin = Address::generate(&env);

    h.aggregator.propose_admin(&h.admin, &new_admin);

    // Check event emitted
    let events = env.events().all();
    let (_, topics, data) = events.last().unwrap();
    assert_eq!(
        topics,
        (soroban_sdk::Symbol::new(&env, "admin_nominated"),).into_val(&env)
    );
    let (version, (from, to)): (u32, (Address, Address)) = data.into_val(&env);
    assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
    assert_eq!(from, h.admin);
    assert_eq!(to, new_admin);

    // Accept admin
    h.aggregator.accept_admin(&new_admin);

    let events2 = env.events().all();
    let (_, topics2, data2) = events2.last().unwrap();
    assert_eq!(
        topics2,
        (soroban_sdk::Symbol::new(&env, "admin_changed"),).into_val(&env)
    );
    let (version2, (accepted,)): (u32, (Address,)) = data2.into_val(&env);
    assert_eq!(version2, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
    assert_eq!(accepted, new_admin);

    assert_eq!(h.aggregator.get_admin(), new_admin);
}

#[test]
#[should_panic(expected = "Error(Contract, #12)")] // WrongAdmin
fn test_admin_rotation_rejected_non_proposed() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let new_admin = Address::generate(&env);
    let rando = Address::generate(&env);

    h.aggregator.propose_admin(&h.admin, &new_admin);
    h.aggregator.accept_admin(&rando);
}

#[test]
#[should_panic(expected = "Error(Contract, #11)")] // NoPendingAdmin
fn test_admin_rotation_accept_without_proposal() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let rando = Address::generate(&env);

    h.aggregator.accept_admin(&rando);
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")] // NotAdmin
fn test_admin_rotation_propose_wrong_admin() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let new_admin = Address::generate(&env);
    let rando = Address::generate(&env);

    h.aggregator.propose_admin(&rando, &new_admin);
}

#[test]
fn test_admin_rotation_old_admin_retains_control() {
    let env = Env::default();
    let h = deploy(&env, 600);
    let new_admin = Address::generate(&env);

    h.aggregator.propose_admin(&h.admin, &new_admin);

    // Old admin can still set max staleness
    h.aggregator.set_max_staleness(&h.admin, &1200);
    assert_eq!(h.aggregator.get_max_staleness(), 1200);
}

#[test]
fn test_pause_and_unpause_requires_admin() {
    let env = Env::default();
    let h = deploy(&env, 600);

    // Pause as admin
    h.aggregator.pause(&h.admin);
    assert!(h.aggregator.is_paused());

    // Unpause as admin
    h.aggregator.unpause(&h.admin);
    assert!(!h.aggregator.is_paused());
}

#[test]
#[should_panic(expected = "Error(Contract, #13)")] // Paused error
fn test_get_price_panics_when_paused() {
    let env = Env::default();
    let h = deploy(&env, 600);
    h.aggregator.pause(&h.admin);

    // Should panic with Paused
    h.aggregator.get_price(&h.token_a, &h.token_b);
}

#[test]
fn test_read_views_callable_when_paused() {
    let env = Env::default();
    let h = deploy(&env, 600);
    h.aggregator.pause(&h.admin);

    // Read-only views remain callable
    let _ = h.aggregator.get_price_safe(&h.token_a, &h.token_b);
    let _ = h.aggregator.get_price_detailed(&h.token_a, &h.token_b);
    let _ = h.aggregator.get_price_spread_bps(&h.token_a, &h.token_b);
    let _ = h.aggregator.list_sources();
    let _ = h.aggregator.get_max_staleness();
    let _ = h.aggregator.get_max_deviation_bps();
    let _ = h.aggregator.get_admin();
}
