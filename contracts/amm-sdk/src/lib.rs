//! # Soroban AMM SDK
//!
//! Type-safe Rust SDK for interacting with the Soroban AMM contracts.
//!
//! The SDK provides three layers:
//!
//! * **`types`** – shared Soroban-compatible types that mirror on-chain data
//!   structures (errors, pool state, swap results, events).
//! * **`client`** – a high-level [`client::AmmPoolSdk`] client that wraps every
//!   contract entry point with Rust-native ergonomics and validated quote
//!   helpers.
//! * **`events`** – strongly-typed event decoders for every event emitted by
//!   the AMM contracts.
//!
//! ## Quick start
//!
//! ```rust,ignore
//! use soroban_amm_sdk::client::AmmPoolSdk;
//! use soroban_sdk::{Address, Env};
//!
//! let env = Env::default();
//! let pool_address: Address = /* … */;
//! let sdk = AmmPoolSdk::new(&env, &pool_address);
//!
//! // Type-safe quote
//! let quote = sdk.quote_swap_in(&token_a, 1_000_000)?;
//! println!("out: {}, impact bps: {}", quote.amount_out, quote.price_impact_bps);
//! ```

#![no_std]
#![allow(unexpected_cfgs)]

pub mod client;
pub mod events;
pub mod types;

#[cfg(all(test, feature = "testutils"))]
mod examples;

#[cfg(all(test, feature = "testutils"))]
mod version_test;

// ── Event schema versioning (#302) ──────────────────────────────────────────
//
// Every event the AMM / CL / governance / factory contracts emit
// goes through `emit_versioned_event!`, which stamps the payload
// with `EVENT_SCHEMA_VERSION` at index 0. Indexers / off-chain
// consumers read `(version, ...rest)`:
//
//   - On startup, refuse versions newer than the one the consumer
//     was compiled against (or fall back to "drop event" depending
//     on policy).
//   - Bump `EVENT_SCHEMA_VERSION` when ANY event payload changes
//     shape (added field, renamed field, type change).
//
// The version is intentionally a single global, not per-event. A
// per-event version would let one event's shape drift independently
// of the others, which makes consumer logic harder to maintain. One
// version-bump per release matches how the contracts are deployed
// (a workspace-wide soroban release).

pub const EVENT_SCHEMA_VERSION: u32 = 1;

/// Stamp a contract event with the current `EVENT_SCHEMA_VERSION`
/// and publish it. Drop-in replacement for `env.events().publish(...)`
/// at every emit site in the AMM / CL / governance / factory crates.
///
/// Expansion:
///
/// ```ignore
/// emit_versioned_event!(env, (topic,), (a, b, c));
/// // → env.events().publish((topic,), (EVENT_SCHEMA_VERSION, (a, b, c)));
/// ```
///
/// Consumers read `(version: u32, payload)` and pattern-match on
/// `version` to pick the right decoder.
#[macro_export]
macro_rules! emit_versioned_event {
    ($env:expr, $topic:expr, $payload:expr) => {{
        $env.events()
            .publish($topic, ($crate::EVENT_SCHEMA_VERSION, $payload));
    }};
}

#[cfg(all(test, feature = "testutils"))]
mod events_test {
    use crate::events::{self, AmmEvent};
    use crate::EVENT_SCHEMA_VERSION;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{symbol_short, Address, Bytes, Env, IntoVal, Symbol, Val, Vec};

    fn decode_published(
        env: &Env,
        topic: Symbol,
        payload: impl IntoVal<Env, Val>,
    ) -> Option<AmmEvent> {
        crate::emit_versioned_event!(env, (topic,), payload);
        let events = env.events().all();
        let event = events.get(events.len() - 1).unwrap();
        events::decode_amm_event(event.topics, event.data)
    }

    #[test]
    fn test_swap_event_round_trip() {
        let env = Env::default();
        let trader = Address::generate(&env);
        let token_in = Address::generate(&env);
        let token_out = Address::generate(&env);
        let result = decode_published(
            &env,
            symbol_short!("swap"),
            (
                trader.clone(),
                token_in.clone(),
                token_out.clone(),
                10_i128,
                20_i128,
                30_i128,
            ),
        );
        match result {
            Some(AmmEvent::Swap(t, ti, to, ai, ao, f)) => {
                assert_eq!(t, trader);
                assert_eq!(ti, token_in);
                assert_eq!(to, token_out);
                assert_eq!(ai, 10);
                assert_eq!(ao, 20);
                assert_eq!(f, 30);
            }
            _ => panic!("expected swap event"),
        }
    }

    #[test]
    fn test_add_liquidity_event_round_trip() {
        let env = Env::default();
        let user = Address::generate(&env);
        let token_a = Address::generate(&env);
        let token_b = Address::generate(&env);
        let result = decode_published(
            &env,
            Symbol::new(&env, "add_liquidity"),
            (
                user.clone(),
                token_a.clone(),
                token_b.clone(),
                100_i128,
                200_i128,
                300_i128,
            ),
        );
        assert!(matches!(result, Some(AmmEvent::AddLiquidity(..))));
    }

    #[test]
    fn test_remove_liquidity_event_round_trip() {
        let env = Env::default();
        let user = Address::generate(&env);
        let token_a = Address::generate(&env);
        let token_b = Address::generate(&env);
        let result = decode_published(
            &env,
            Symbol::new(&env, "remove_liquidity"),
            (user, token_a, token_b, 1000_i128, 2000_i128, 3000_i128),
        );
        assert!(matches!(result, Some(AmmEvent::RemoveLiquidity(..))));
    }

    #[test]
    fn test_remove_liquidity_one_sided_event_round_trip() {
        let env = Env::default();
        let user = Address::generate(&env);
        let token = Address::generate(&env);
        let result = decode_published(
            &env,
            Symbol::new(&env, "remove_liquidity_one_sided"),
            (user, token, 500_i128, 200_i128),
        );
        assert!(matches!(
            result,
            Some(AmmEvent::RemoveLiquidityOneSided(..))
        ));
    }

    #[test]
    fn test_flash_loan_event_round_trip() {
        let env = Env::default();
        let borrower = Address::generate(&env);
        let token = Address::generate(&env);
        let result = decode_published(
            &env,
            Symbol::new(&env, "flash_loan"),
            (borrower, token, 10000_i128, 50_i128),
        );
        assert!(matches!(result, Some(AmmEvent::FlashLoan(..))));
    }

    #[test]
    fn test_fee_updated_event_round_trip() {
        let env = Env::default();
        let result = decode_published(&env, Symbol::new(&env, "fee_updated"), (100u32,));
        assert!(matches!(result, Some(AmmEvent::FeeUpdated(..))));
    }

    #[test]
    fn test_flash_fee_updated_event_round_trip() {
        let env = Env::default();
        let result = decode_published(&env, Symbol::new(&env, "flash_fee_updated"), (50u32,));
        assert!(matches!(result, Some(AmmEvent::FlashFeeUpdated(..))));
    }

    #[test]
    fn test_admin_nominated_event_round_trip() {
        let env = Env::default();
        let new_admin = Address::generate(&env);
        let result = decode_published(
            &env,
            Symbol::new(&env, "admin_nominated"),
            (new_admin.clone(),),
        );
        match result {
            Some(AmmEvent::AdminNominated(admin)) => assert_eq!(admin, new_admin),
            _ => panic!("expected admin_nominated event"),
        }
    }

    #[test]
    fn test_admin_changed_event_round_trip() {
        let env = Env::default();
        let old_admin = Address::generate(&env);
        let new_admin = Address::generate(&env);
        let result = decode_published(
            &env,
            Symbol::new(&env, "admin_changed"),
            (old_admin, new_admin),
        );
        assert!(matches!(result, Some(AmmEvent::AdminChanged(..))));
    }

    #[test]
    fn test_upgraded_event_round_trip() {
        let env = Env::default();
        let new_contract = Bytes::from_slice(&env, &[1, 2, 3]);
        let result = decode_published(&env, symbol_short!("upgraded"), (new_contract.clone(),));
        match result {
            Some(AmmEvent::Upgraded(contract)) => assert_eq!(contract, new_contract),
            _ => panic!("expected upgraded event"),
        }
    }

    #[test]
    fn test_circuit_breaker_event_round_trip() {
        let env = Env::default();
        let token = Address::generate(&env);
        let code = 3u32;
        let result = decode_published(
            &env,
            Symbol::new(&env, "circuit_breaker"),
            (token.clone(), code),
        );
        match result {
            Some(AmmEvent::CircuitBreaker(t, c)) => {
                assert_eq!(t, token);
                assert_eq!(c, code);
            }
            _ => panic!("expected circuit_breaker event"),
        }
    }

    #[test]
    fn test_empty_topics_returns_none() {
        let env = Env::default();
        let topics = Vec::new(&env);
        let data = (EVENT_SCHEMA_VERSION, (1_i128,)).into_val(&env);
        assert!(events::decode_amm_event(topics, data).is_none());
    }

    #[test]
    fn test_wrong_version_returns_none() {
        let env = Env::default();
        let topic = symbol_short!("swap");
        env.events()
            .publish((topic,), (EVENT_SCHEMA_VERSION + 1, (1_i128,)));
        let events = env.events().all();
        let event = events.get(0).unwrap();
        assert!(events::decode_amm_event(event.topics, event.data).is_none());
    }

    #[test]
    fn test_unrecognized_symbol_returns_none() {
        let env = Env::default();
        let topic = symbol_short!("unknown");
        env.events()
            .publish((topic,), (EVENT_SCHEMA_VERSION, (1_i128,)));
        let events = env.events().all();
        let event = events.get(0).unwrap();
        assert!(events::decode_amm_event(event.topics, event.data).is_none());
    }

    #[test]
    fn test_mismatched_payload_returns_none() {
        let env = Env::default();
        let topic = symbol_short!("swap");
        // swap expects 6 fields, this provides only 1
        env.events()
            .publish((topic,), (EVENT_SCHEMA_VERSION, (1_i128,)));
        let events = env.events().all();
        let event = events.get(0).unwrap();
        assert!(events::decode_amm_event(event.topics, event.data).is_none());
    }
}
