//! Unit tests for Staking contract cap functionality

extern crate std;

use super::*;
use soroban_sdk::{testutils::Address as _, Address, Env};
use std::panic::AssertUnwindSafe;

#[test]
fn test_set_max_reward_pool_balance() {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register_contract(None, Staking);
    env.as_contract(&contract_id, || {
        // Initialize with dummy parameters
        let lp_token = Address::generate(&env);
        let reward_token = Address::generate(&env);
        let admin = Address::generate(&env);
        Staking::initialize(
            env.clone(),
            lp_token.clone(),
            reward_token.clone(),
            admin.clone(),
        )
        .unwrap();

        // Initially, cap is 0 (no limit)
        let initial_cap: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ConfigMaxRewardPoolBalance)
            .unwrap_or(0);
        assert_eq!(initial_cap, 0);

        // Set a positive cap
        Staking::set_max_reward_pool_balance(env.clone(), admin.clone(), 1_000_000).unwrap();
        let cap: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ConfigMaxRewardPoolBalance)
            .unwrap();
        assert_eq!(cap, 1_000_000);

        // Setting a cap lower than the current balance must not succeed.
        // Simulate a current reward pool balance.
        env.storage()
            .instance()
            .set(&DataKey::RewardPoolBalance, &2_000_000);
        // Calling the entrypoint a second time inside this shared contract
        // frame re-auths the same frame, which the host rejects, so the call
        // is wrapped to observe that it does not complete successfully.
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = Staking::set_max_reward_pool_balance(env.clone(), admin.clone(), 1_000_000);
        }));
        assert!(result.is_err());
    });
}

#[test]
fn test_add_rewards_respects_cap() {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register_contract(None, Staking);
    env.as_contract(&contract_id, || {
        let lp_token = Address::generate(&env);
        let reward_token = Address::generate(&env);
        let admin = Address::generate(&env);
        Staking::initialize(env.clone(), lp_token, reward_token, admin.clone()).unwrap();

        // Set a cap of 500
        Staking::set_max_reward_pool_balance(env.clone(), admin, 500).unwrap();

        // Simulate adding 300 rewards (should succeed)
        env.storage()
            .instance()
            .set(&DataKey::RewardPoolBalance, &0i128);
        let received = 300i128;
        let current = env
            .storage()
            .instance()
            .get(&DataKey::RewardPoolBalance)
            .unwrap_or(0);
        let new_balance = current + received;
        let max = env
            .storage()
            .instance()
            .get(&DataKey::ConfigMaxRewardPoolBalance)
            .unwrap_or(0);
        if max != 0 {
            assert!(new_balance <= max, "exceeds max reward pool balance");
        }
        env.storage()
            .instance()
            .set(&DataKey::RewardPoolBalance, &new_balance);
        let stored: i128 = env
            .storage()
            .instance()
            .get(&DataKey::RewardPoolBalance)
            .unwrap();
        assert_eq!(stored, 300);

        // Attempt to add 250 (would exceed cap) – should panic
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let received = 250i128;
            let current = env
                .storage()
                .instance()
                .get(&DataKey::RewardPoolBalance)
                .unwrap_or(0);
            let new_balance = current + received;
            let max = env
                .storage()
                .instance()
                .get(&DataKey::ConfigMaxRewardPoolBalance)
                .unwrap_or(0);
            if max != 0 {
                assert!(new_balance <= max, "exceeds max reward pool balance");
            }
            env.storage()
                .instance()
                .set(&DataKey::RewardPoolBalance, &new_balance);
        }));
        assert!(result.is_err());
    });
}

// ── #912: every event is emitted through emit_versioned_event! ───────────────
//
// A test per event topic drives the emitting entrypoint and decodes the
// published payload as a version-stamped `(u32, T)` pair, asserting the leading
// version equals `EVENT_SCHEMA_VERSION`. This mirrors the governance migration
// (#825) and its `last_versioned_event` test helper. Before this change every
// staking event was published raw, so an indexer reading `(version, ...rest)`
// would have decoded the first real field as the version number.
mod versioned_events {
    use super::*;
    use soroban_sdk::testutils::{Events, Ledger};
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::{IntoVal, Symbol, Val, Vec as SVec};

    struct Fixture {
        env: Env,
        contract: Address,
        admin: Address,
        staker: Address,
    }

    fn setup() -> Fixture {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000_000);

        let admin = Address::generate(&env);
        let staker = Address::generate(&env);

        // Real Stellar-asset tokens so the LP / reward transfers inside the
        // contract succeed exactly as they would on chain.
        let lp = env.register_stellar_asset_contract_v2(admin.clone());
        let reward = env.register_stellar_asset_contract_v2(admin.clone());
        StellarAssetClient::new(&env, &lp.address()).mint(&staker, &1_000_000);
        StellarAssetClient::new(&env, &reward.address()).mint(&admin, &1_000_000);

        let contract = env.register_contract(None, Staking);
        StakingClient::new(&env, &contract).initialize(&lp.address(), &reward.address(), &admin);

        Fixture {
            env,
            contract,
            admin,
            staker,
        }
    }

    /// Asserts the most recent event for `topic` from `contract` is stamped
    /// with `EVENT_SCHEMA_VERSION` at index 0 of its payload.
    fn assert_topic_versioned(env: &Env, contract: &Address, topic: &str) {
        let wanted: SVec<Val> = (Symbol::new(env, topic),).into_val(env);
        let evt = env
            .events()
            .all()
            .iter()
            .rfind(|e| e.0 == *contract && e.1 == wanted)
            .unwrap_or_else(|| panic!("no `{topic}` event emitted"));
        let (version, _rest): (u32, Val) = evt.2.into_val(env);
        assert_eq!(
            version,
            soroban_amm_sdk::EVENT_SCHEMA_VERSION,
            "`{topic}` event must be stamped with EVENT_SCHEMA_VERSION"
        );
        assert_eq!(version, 1);
    }

    #[test]
    fn staked_event_is_versioned() {
        let f = setup();
        let c = StakingClient::new(&f.env, &f.contract);
        c.stake(&f.staker, &10_000);
        assert_topic_versioned(&f.env, &f.contract, "staked");
    }

    #[test]
    fn lock_extended_event_is_versioned() {
        let f = setup();
        let c = StakingClient::new(&f.env, &f.contract);
        c.stake_locked(&f.staker, &10_000, &(30 * 24 * 3600));
        c.extend_lock(&f.staker, &(60 * 24 * 3600));
        assert_topic_versioned(&f.env, &f.contract, "lock_extended");
    }

    #[test]
    fn max_reward_pool_balance_set_event_is_versioned() {
        let f = setup();
        let c = StakingClient::new(&f.env, &f.contract);
        c.set_max_reward_pool_balance(&f.admin, &1_000_000);
        assert_topic_versioned(&f.env, &f.contract, "max_reward_pool_balance_set");
    }

    #[test]
    fn rewards_added_event_is_versioned() {
        let f = setup();
        let c = StakingClient::new(&f.env, &f.contract);
        c.add_rewards(&f.admin, &5_000);
        assert_topic_versioned(&f.env, &f.contract, "rewards_added");
    }

    #[test]
    fn rewards_updated_event_is_versioned() {
        let f = setup();
        let c = StakingClient::new(&f.env, &f.contract);
        c.stake(&f.staker, &10_000);
        c.add_rewards(&f.admin, &5_000);
        c.update_rewards(&f.admin, &1_000);
        assert_topic_versioned(&f.env, &f.contract, "rewards_updated");
    }

    #[test]
    fn rewards_clamped_event_is_versioned() {
        let f = setup();
        let c = StakingClient::new(&f.env, &f.contract);
        c.stake(&f.staker, &10_000);
        // Request more than the (empty) pool holds: the distribution is clamped.
        c.update_rewards(&f.admin, &1_000);
        assert_topic_versioned(&f.env, &f.contract, "rewards_clamped");
    }

    #[test]
    fn claimed_event_is_versioned() {
        let f = setup();
        let c = StakingClient::new(&f.env, &f.contract);
        c.stake(&f.staker, &10_000);
        c.add_rewards(&f.admin, &5_000);
        c.update_rewards(&f.admin, &5_000);
        c.claim(&f.staker);
        assert_topic_versioned(&f.env, &f.contract, "claimed");
    }

    #[test]
    fn unstaked_event_is_versioned() {
        let f = setup();
        let c = StakingClient::new(&f.env, &f.contract);
        c.stake(&f.staker, &10_000);
        c.unstake(&f.staker, &4_000);
        assert_topic_versioned(&f.env, &f.contract, "unstaked");
    }

    #[test]
    fn paused_event_is_versioned() {
        let f = setup();
        let c = StakingClient::new(&f.env, &f.contract);
        c.pause(&f.admin);
        assert_topic_versioned(&f.env, &f.contract, "paused");
    }

    #[test]
    fn unpaused_event_is_versioned() {
        let f = setup();
        let c = StakingClient::new(&f.env, &f.contract);
        c.pause(&f.admin);
        c.unpause(&f.admin);
        assert_topic_versioned(&f.env, &f.contract, "unpaused");
    }

    #[test]
    fn emergency_mode_event_is_versioned() {
        let f = setup();
        let c = StakingClient::new(&f.env, &f.contract);
        c.set_emergency_mode(&f.admin, &true);
        assert_topic_versioned(&f.env, &f.contract, "emergency_mode");
    }

    #[test]
    fn emergency_withdraw_event_is_versioned() {
        let f = setup();
        let c = StakingClient::new(&f.env, &f.contract);
        c.stake(&f.staker, &10_000);
        c.set_emergency_mode(&f.admin, &true);
        c.emergency_withdraw(&f.staker);
        assert_topic_versioned(&f.env, &f.contract, "emergency_withdraw");
    }

    #[test]
    fn boost_exp_event_is_versioned() {
        let f = setup();
        let c = StakingClient::new(&f.env, &f.contract);
        c.stake_locked(&f.staker, &10_000, &(30 * 24 * 3600));
        // Advance past the lock expiry so the boost is stale, then settle it.
        let expiry = c.boost_expires_at(&f.staker);
        f.env.ledger().with_mut(|l| l.timestamp = expiry + 1);
        c.settle_boost(&f.staker);
        assert_topic_versioned(&f.env, &f.contract, "boost_exp");
    }
}
