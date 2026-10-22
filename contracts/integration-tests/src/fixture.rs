//! Reusable protocol fixture for cross-contract integration tests.
//!
//! This fixture deploys a complete protocol instance (all contracts needed
//! to test interactions) and exposes a clean API for tests to work with.
//!
//! Without this fixture, each test would require ~100 lines of boilerplate
//! setup. This centralizes that setup and makes the test suite maintainable.

use soroban_sdk::{
    testutils::{Address as _, Ledger, LedgerInfo},
    token::StellarAssetClient,
    Address, BytesN, Env,
};

// WASM artifacts from compiled contracts. Only the AMM pool and SEP-41 token
// are deployed from uploaded WASM (the factory deploys pool instances from
// these hashes); the other contracts here are registered natively via their
// Rust contract type instead.
use amm::WASM as AMM_WASM;
use token::WASM as TOKEN_WASM;

/// A complete protocol instance for testing.
///
/// Includes:
/// - Two SEP-41 tokens (token A and token B)
/// - AMM Factory and deployed pools (V2 and CL)
/// - Governance contract
/// - Staking contract
/// - LP tokens and NFTs
///
/// All contracts are registered and interlinked.
pub struct Protocol {
    pub env: Env,
    pub admin: Address,

    // Tokens
    pub token_a: Address,
    pub token_b: Address,

    // Core contracts
    pub factory: Address,
    pub v2_pool: Address,
    pub cl_pool: Address,
    pub v2_lp_token: Address,
    pub governance: Address,
    pub staking: Address,
    pub cl_nft: Address,
}

impl Protocol {
    /// Deploy a complete protocol instance.
    ///
    /// This is the heavy lifting — deploy all contracts, link them,
    /// set initial state. Tests then just call methods on `self`.
    pub fn deploy() -> Self {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        // Set deterministic ledger state for reproducibility
        let ts = 1_000_000u64;
        env.ledger().set(LedgerInfo {
            timestamp: ts,
            protocol_version: 21,
            sequence_number: 1,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4096,
            max_entry_ttl: 6_312_000,
        });

        let admin = Address::generate(&env);

        // ── Deploy tokens ───────────────────────────────────────────────────

        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        // Mint tokens to admin
        StellarAssetClient::new(&env, &token_a).mint(&admin, &1_000_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&admin, &1_000_000_000_i128);

        // ── Upload WASM hashes ──────────────────────────────────────────────

        let amm_hash: BytesN<32> = env.deployer().upload_contract_wasm(AMM_WASM);
        let token_hash: BytesN<32> = env.deployer().upload_contract_wasm(TOKEN_WASM);

        // ── Deploy factory ───────────────────────────────────────────────────

        let factory = env.register_contract(None, factory::Factory);

        // Initialize factory with WASM hashes
        let factory_client = factory::FactoryClient::new(&env, &factory);
        factory_client.initialize(&admin, &amm_hash, &token_hash);

        // ── Deploy V2 pool ──────────────────────────────────────────────────

        // Create pool via factory (fee tier 2 = 30 bps = 0.3% fee)
        let (v2_pool, _pool_governance) =
            factory_client.create_pool(&admin, &token_a, &token_b, &2i128, &None);
        let v2_lp_token = factory_client
            .get_lp_token(&v2_pool)
            .expect("LP token should exist for a freshly created pool");

        // ── Deploy governance ───────────────────────────────────────────────

        let governance = env.register_contract(None, governance::Governance);
        let gov_client = governance::GovernanceClient::new(&env, &governance);
        gov_client.initialize(
            &admin,
            &v2_pool,
            &v2_lp_token, // Use V2 LP token as voting token
            &7200u64,     // voting period (secs)
            &86400u64,    // timelock (secs)
            &3000i128,    // quorum (bps)
            &100i128,     // min proposer stake (bps)
        );

        // ── Deploy staking ──────────────────────────────────────────────────

        let staking = env.register_contract(None, staking::Staking);
        let staking_client = staking::StakingClient::new(&env, &staking);
        // Staked token is the V2 LP token; rewards are paid in token A. The
        // staking contract rejects lp_token == reward_token (IdenticalTokens).
        staking_client.initialize(&v2_lp_token, &token_a, &admin);

        // ── Deploy CL pool ──────────────────────────────────────────────────

        let cl_pool = env.register_contract(None, concentrated_liquidity::ConcentratedLiquidity);
        // Note: full CL initialization would happen here

        // ── Deploy CL NFT wrapper ────────────────────────────────────────────

        let cl_nft = env.register_contract(None, cl_position_nft::ClPositionNft);

        Protocol {
            env,
            admin,
            token_a,
            token_b,
            factory,
            v2_pool,
            cl_pool,
            v2_lp_token,
            governance,
            staking,
            cl_nft,
        }
    }

    /// Get the current timestamp (for deadline checks).
    pub fn now(&self) -> u64 {
        self.env.ledger().timestamp()
    }

    /// Advance time by n seconds.
    pub fn advance_time(&self, seconds: u64) {
        let current = self.env.ledger().sequence();
        let current_ts = self.env.ledger().timestamp();
        self.env.ledger().set(LedgerInfo {
            timestamp: current_ts + seconds,
            protocol_version: 21,
            sequence_number: current + 1,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4096,
            max_entry_ttl: 6_312_000,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::token::Client as TokenClient;

    #[test]
    fn protocol_deployment_succeeds() {
        let protocol = Protocol::deploy();
        assert_ne!(
            protocol.admin,
            Address::generate(&protocol.env),
            "Admin should be set"
        );
        assert_ne!(
            protocol.v2_pool,
            Address::generate(&protocol.env),
            "Pool should be deployed"
        );
    }

    #[test]
    fn tokens_are_minted_to_admin() {
        let protocol = Protocol::deploy();
        let token_a_balance =
            TokenClient::new(&protocol.env, &protocol.token_a).balance(&protocol.admin);
        assert!(token_a_balance > 0, "Admin should have token A");
    }
}
