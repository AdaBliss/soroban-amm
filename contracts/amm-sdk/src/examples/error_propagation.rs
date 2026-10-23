//! Regression test for issue #477: state-mutating `AmmPoolSdk` wrappers must
//! return real `Err` values for on-chain contract errors instead of calling
//! the panicking, non-`try_` client methods.
//!
//! Run with:
//! ```
//! cargo test -p soroban_amm_sdk -- examples::error_propagation
//! ```

#[cfg(test)]
#[allow(clippy::module_inception)]
mod error_propagation {
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::StellarAssetClient,
        Address, Env,
    };

    use crate::client::AmmPoolSdk;
    use crate::types::SdkAmmError;

    /// Sets up a seeded pool and returns everything a test needs. Mirrors
    /// `examples::basic_swap`'s setup so the two examples stay consistent.
    struct Setup {
        env: Env,
        amm_id: Address,
        ta_addr: Address,
    }

    fn setup() -> Setup {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000_000);

        let admin = Address::generate(&env);
        let amm_id = env.register_contract(None, amm::AmmPool);
        let lp_id = env.register_contract(None, token::LpToken);

        token::LpTokenClient::new(&env, &lp_id).initialize(
            &amm_id,
            &soroban_sdk::String::from_str(&env, "LP"),
            &soroban_sdk::String::from_str(&env, "LP"),
            &7u32,
        );

        let register_sac = |admin: &Address| {
            let c = env.register_stellar_asset_contract_v2(admin.clone());
            (
                soroban_sdk::token::TokenClient::new(&env, &c.address()),
                StellarAssetClient::new(&env, &c.address()),
            )
        };
        let (ta, ta_sac) = register_sac(&admin);
        let (tb, tb_sac) = register_sac(&admin);

        amm::AmmPoolClient::new(&env, &amm_id).initialize(
            &admin,
            &ta.address,
            &tb.address,
            &lp_id,
            &30_i128,
            &admin,
            &0_i128,
        );

        let provider = Address::generate(&env);
        ta_sac.mint(&provider, &10_000_000_i128);
        tb_sac.mint(&provider, &10_000_000_i128);

        let sdk = AmmPoolSdk::new(&env, &amm_id);
        sdk.add_liquidity(&provider, 10_000_000, 10_000_000, 0, u64::MAX)
            .unwrap();

        let _ = tb;
        Setup {
            env,
            amm_id,
            ta_addr: ta.address,
        }
    }

    /// A paused pool must surface as `Err(SdkAmmError::Paused)`, not a panic.
    #[test]
    fn execute_swap_returns_err_when_paused() {
        let s = setup();
        amm::AmmPoolClient::new(&s.env, &s.amm_id).pause();

        let sdk = AmmPoolSdk::new(&s.env, &s.amm_id);
        let trader = Address::generate(&s.env);
        let result = sdk.execute_swap(&trader, &s.ta_addr, 1_000, 0, u64::MAX);

        assert_eq!(result, Err(SdkAmmError::Paused));
    }

    /// A slippage violation must surface as `Err(SdkAmmError::SlippageExceeded)`.
    #[test]
    fn execute_swap_returns_err_on_slippage() {
        let s = setup();
        let trader = Address::generate(&s.env);
        StellarAssetClient::new(&s.env, &s.ta_addr).mint(&trader, &1_000_000_i128);

        let sdk = AmmPoolSdk::new(&s.env, &s.amm_id);
        // An unreachably high min_out guarantees the on-chain slippage guard trips.
        let result = sdk.execute_swap(&trader, &s.ta_addr, 1_000, i128::MAX, u64::MAX);

        assert_eq!(result, Err(SdkAmmError::SlippageExceeded));
    }

    /// Removing more shares than owned must surface as `Err`, not a panic.
    #[test]
    fn remove_liquidity_returns_err_on_insufficient_shares() {
        let s = setup();
        let sdk = AmmPoolSdk::new(&s.env, &s.amm_id);
        let stranger = Address::generate(&s.env);

        let result = sdk.remove_liquidity(&stranger, 1, 0, 0, u64::MAX);

        assert_eq!(result, Err(SdkAmmError::InsufficientShares));
    }
}
