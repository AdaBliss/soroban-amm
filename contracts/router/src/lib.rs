//! Multi-hop swap router.
//!
//! Routes swaps through one or more AMM pools discovered via the factory
//! contract. A path is an ordered list of token addresses where each adjacent
//! pair must have a deployed pool.

#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, BytesN, Env, Vec,
};

use pool_interfaces::{AmmPoolClient, FactoryClient};

const MIN_TTL: u32 = 172_800;
const BUMP_TO: u32 = 518_400;

// ── Typed errors ─────────────────────────────────────────────────────────────

/// Errors surfaced by the router.
///
/// Every caller-triggerable failure path returns one of these discriminants
/// instead of trapping, so integrators can branch on the specific reason a
/// multi-hop route failed. Discriminants 1 and 2 follow the workspace-wide
/// convention (`AlreadyInitialized` / `NotInitialized`); see
/// `contracts/amm/src/lib.rs` `AmmError` for the reference enum.
#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum RouterError {
    /// `initialize` was called on a router that is already set up.
    AlreadyInitialized = 1,
    /// A function was called before `initialize`.
    NotInitialized = 2,
    /// The path has fewer than two tokens.
    InvalidPath = 3,
    /// Two adjacent tokens in the path are identical; no such pool can exist.
    DuplicateAdjacentToken = 4,
    /// An input or output amount was zero or negative.
    InvalidAmount = 5,
    /// The transaction deadline has already passed.
    DeadlineExceeded = 6,
    /// A hop in the path has no deployed pool.
    PoolNotFound = 7,
    /// The realized amount violated the caller's slippage bound.
    SlippageExceeded = 8,
    /// A swap was attempted while the router is paused by its admin.
    Paused = 9,
    /// `pause` / `unpause` was called with an address that is not the stored
    /// admin.
    Unauthorized = 10,
    /// The supplied factory does not implement the factory interface.
    InvalidFactory = 11,
    /// `accept_admin` was called without a pending nomination.
    NoPendingAdmin = 12,
}

#[contracttype]
pub enum DataKey {
    Factory,
    Admin,
    PendingAdmin,
    /// `true` while swaps are halted by the admin; absent means unpaused.
    Paused,
}

#[contract]
pub struct Router;

#[contractimpl]
impl Router {
    /// Initialize the router with the factory that tracks all deployed pools.
    pub fn initialize(env: Env, admin: Address, factory: Address) -> Result<(), RouterError> {
        Self::extend_ttl(&env);
        if env.storage().instance().has(&DataKey::Factory) {
            return Err(RouterError::AlreadyInitialized);
        }
        admin.require_auth();
        if FactoryClient::new(&env, &factory)
            .try_get_pool_count()
            .is_err()
        {
            return Err(RouterError::InvalidFactory);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Factory, &factory);
        Ok(())
    }

    /// Halt every swap entrypoint. Admin-only.
    ///
    /// Quotes and path views stay callable so integrators can still read
    /// routes while the router is paused.
    pub fn pause(env: Env, admin: Address) -> Result<(), RouterError> {
        Self::extend_ttl(&env);
        Self::require_admin(&env, &admin)?;
        env.storage().instance().set(&DataKey::Paused, &true);
        soroban_amm_sdk::emit_versioned_event!(env, (symbol_short!("pause"),), (admin,));
        Ok(())
    }

    /// Resume swaps after `pause`. Admin-only.
    pub fn unpause(env: Env, admin: Address) -> Result<(), RouterError> {
        Self::extend_ttl(&env);
        Self::require_admin(&env, &admin)?;
        env.storage().instance().set(&DataKey::Paused, &false);
        soroban_amm_sdk::emit_versioned_event!(env, (symbol_short!("unpause"),), (admin,));
        Ok(())
    }

    /// Whether swaps are currently halted.
    pub fn is_paused(env: Env) -> bool {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Execute a multi-hop swap along `path`.
    pub fn swap_exact_in(
        env: Env,
        trader: Address,
        path: Vec<Address>,
        amount_in: i128,
        min_amount_out: i128,
        deadline: u64,
    ) -> Result<i128, RouterError> {
        Self::extend_ttl(&env);
        Self::require_not_paused(&env)?;
        trader.require_auth();
        Self::require_valid_path(&path)?;
        if amount_in <= 0 {
            return Err(RouterError::InvalidAmount);
        }

        if env.ledger().timestamp() > deadline {
            return Err(RouterError::DeadlineExceeded);
        }

        let factory_client = Self::factory_client(&env)?;

        let mut current_amount = amount_in;
        let hops = path.len() - 1;

        for i in 0..hops {
            let token_in = path.get(i).unwrap();
            let pool = Self::pool_for_hop(&factory_client, &path, i)?;

            // Intermediate hops carry no floor of their own; the router enforces
            // the overall slippage bound against `min_amount_out` after the last
            // hop, so only that hop passes a non-zero minimum to the pool.
            let hop_min_out = if i + 1 == hops { min_amount_out } else { 0 };

            current_amount = AmmPoolClient::new(&env, &pool).swap(
                &trader,
                &token_in,
                &current_amount,
                &hop_min_out,
                &deadline,
            );
        }

        if current_amount < min_amount_out {
            return Err(RouterError::SlippageExceeded);
        }

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("route"),),
            (trader, path, amount_in, current_amount, hops)
        );

        Ok(current_amount)
    }

    pub fn swap_exact_out(
        env: Env,
        trader: Address,
        path: Vec<Address>,
        amount_out: i128,
        max_in: i128,
        deadline: u64,
    ) -> Result<i128, RouterError> {
        Self::extend_ttl(&env);
        Self::require_not_paused(&env)?;
        trader.require_auth();
        Self::require_valid_path(&path)?;
        if amount_out <= 0 {
            return Err(RouterError::InvalidAmount);
        }

        if env.ledger().timestamp() > deadline {
            return Err(RouterError::DeadlineExceeded);
        }

        let factory_client = Self::factory_client(&env)?;

        let hops = path.len() - 1;
        // Same reverse walk `get_amounts_in_path` quotes with, so a quote and
        // the execution it precedes can never drift apart.
        let amounts_in = Self::reverse_walk(&env, &factory_client, &path, amount_out)?;

        let total_in = amounts_in.get(0).unwrap();
        if total_in > max_in {
            return Err(RouterError::SlippageExceeded);
        }

        let mut current_amount_in = total_in;
        for i in 0..hops {
            let token_in = path.get(i).unwrap();
            let pool = Self::pool_for_hop(&factory_client, &path, i)?;

            let expected_out = amounts_in.get(i + 1).unwrap();

            let actual_out = AmmPoolClient::new(&env, &pool).swap(
                &trader,
                &token_in,
                &current_amount_in,
                &expected_out,
                &deadline,
            );

            if actual_out < expected_out {
                return Err(RouterError::SlippageExceeded);
            }
            current_amount_in = actual_out;
        }

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("route"),),
            (trader, path, total_in, amount_out, hops)
        );
        Ok(total_in)
    }

    /// Quote the output of a multi-hop swap without executing it.
    ///
    /// Returns `0` when any hop has no deployed pool. That sentinel is
    /// indistinguishable from a route that genuinely produces zero output and
    /// is kept only for ABI compatibility -- call
    /// [`Router::is_path_routable`] first to tell the two apart, or
    /// [`Router::get_amounts_out_path`] for the per-hop breakdown.
    pub fn get_amount_out_path(
        env: Env,
        path: Vec<Address>,
        amount_in: i128,
    ) -> Result<i128, RouterError> {
        Self::extend_ttl(&env);
        Self::require_valid_path(&path)?;
        if amount_in <= 0 {
            return Err(RouterError::InvalidAmount);
        }

        let factory_client = Self::factory_client(&env)?;

        let mut current_amount = amount_in;
        let hops = path.len() - 1;

        for i in 0..hops {
            let token_in = path.get(i).unwrap();
            let token_out = path.get(i + 1).unwrap();

            let pool = match factory_client.get_pool(&token_in, &token_out) {
                Some(p) => p,
                None => return Ok(0),
            };

            current_amount =
                AmmPoolClient::new(&env, &pool).get_amount_out(&token_in, &current_amount);
        }

        Ok(current_amount)
    }

    /// Quote the input required to receive exactly `amount_out` along `path`.
    ///
    /// The exact-out mirror of [`Router::get_amount_out_path`]. Walks the path
    /// in reverse through `AmmPoolClient::get_amount_in`, using the same helper
    /// `swap_exact_out` executes with. Returns `RouterError::PoolNotFound` when
    /// any hop has no deployed pool; use [`Router::is_path_routable`] to check first.
    pub fn get_amount_in_path(
        env: Env,
        path: Vec<Address>,
        amount_out: i128,
    ) -> Result<i128, RouterError> {
        Self::extend_ttl(&env);
        Self::require_valid_path(&path)?;
        if amount_out <= 0 {
            return Err(RouterError::InvalidAmount);
        }

        let factory_client = Self::factory_client(&env)?;
        Ok(
            Self::reverse_walk(&env, &factory_client, &path, amount_out)?
                .get(0)
                .unwrap(),
        )
    }

    /// Per-hop breakdown of an exact-in quote.
    ///
    /// Returns `path.len()` amounts where element `i` is the amount of
    /// `path[i]` moving through the route; element `0` is `amount_in` and the
    /// last element equals [`Router::get_amount_out_path`] for the same
    /// arguments. Returns `RouterError::PoolNotFound` when a hop has no pool,
    /// rather than collapsing the route to the ambiguous `0` sentinel.
    pub fn get_amounts_out_path(
        env: Env,
        path: Vec<Address>,
        amount_in: i128,
    ) -> Result<Vec<i128>, RouterError> {
        Self::extend_ttl(&env);
        Self::require_valid_path(&path)?;
        if amount_in <= 0 {
            return Err(RouterError::InvalidAmount);
        }

        let factory_client = Self::factory_client(&env)?;
        Self::forward_walk(&env, &factory_client, &path, amount_in)
    }

    /// Per-hop breakdown of an exact-out quote.
    ///
    /// Returns `path.len()` amounts where element `i` is the amount of
    /// `path[i]` moving through the route; element `0` is the required input
    /// and the last element is `amount_out`.
    pub fn get_amounts_in_path(
        env: Env,
        path: Vec<Address>,
        amount_out: i128,
    ) -> Result<Vec<i128>, RouterError> {
        Self::extend_ttl(&env);
        Self::require_valid_path(&path)?;
        if amount_out <= 0 {
            return Err(RouterError::InvalidAmount);
        }

        let factory_client = Self::factory_client(&env)?;
        Self::reverse_walk(&env, &factory_client, &path, amount_out)
    }

    /// Resolve every adjacent pair in `path` to its pool address.
    ///
    /// Returns `path.len() - 1` addresses so integrators can inspect fee tiers
    /// and reserves themselves. Returns `RouterError::PoolNotFound` when a
    /// pair has no deployed pool.
    pub fn get_pools_for_path(env: Env, path: Vec<Address>) -> Result<Vec<Address>, RouterError> {
        Self::extend_ttl(&env);
        Self::require_valid_path(&path)?;

        let factory_client = Self::factory_client(&env)?;
        let hops = path.len() - 1;
        let mut pools = Vec::new(&env);
        for i in 0..hops {
            pools.push_back(Self::pool_for_hop(&factory_client, &path, i)?);
        }
        Ok(pools)
    }

    /// Whether every hop in `path` has a deployed pool.
    ///
    /// This is the unambiguous replacement for
    /// [`Router::get_amount_out_path`]'s `0` return: a route that is simply
    /// missing a pool is `false` here, while a routable path that quotes zero
    /// output is `true`. A malformed path (fewer than two tokens, or a repeated
    /// adjacent token) is also `false` -- a predicate reports rather than
    /// panics.
    pub fn is_path_routable(env: Env, path: Vec<Address>) -> bool {
        Self::extend_ttl(&env);
        if path.len() < 2 {
            return false;
        }
        let factory_client = match Self::factory_client(&env) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let hops = path.len() - 1;
        for i in 0..hops {
            let token_in = path.get(i).unwrap();
            let token_out = path.get(i + 1).unwrap();
            if token_in == token_out {
                return false;
            }
            if factory_client.get_pool(&token_in, &token_out).is_none() {
                return false;
            }
        }
        true
    }

    pub fn get_factory(env: Env) -> Result<Address, RouterError> {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Factory)
            .ok_or(RouterError::NotInitialized)
    }

    pub fn propose_admin(
        env: Env,
        current_admin: Address,
        new_admin: Address,
    ) -> Result<(), RouterError> {
        Self::extend_ttl(&env);
        Self::require_admin(&env, &current_admin)?;
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Some(new_admin.clone()));
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (soroban_sdk::Symbol::new(&env, "admin_nominated"),),
            (current_admin, new_admin)
        );
        Ok(())
    }

    pub fn accept_admin(env: Env, new_admin: Address) -> Result<(), RouterError> {
        Self::extend_ttl(&env);
        let pending: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None);
        let nominee = pending.ok_or(RouterError::NoPendingAdmin)?;
        if new_admin != nominee {
            return Err(RouterError::Unauthorized);
        }
        new_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Option::<Address>::None);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (soroban_sdk::Symbol::new(&env, "admin_changed"),),
            (new_admin,)
        );
        Ok(())
    }

    pub fn get_admin(env: Env) -> Result<Address, RouterError> {
        Self::extend_ttl(&env);
        Self::read_admin(&env)
    }

    pub fn get_pending_admin(env: Env) -> Option<Address> {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None)
    }

    pub fn upgrade(env: Env, admin: Address, new_wasm_hash: BytesN<32>) -> Result<(), RouterError> {
        Self::extend_ttl(&env);
        Self::require_admin(&env, &admin)?;
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        soroban_amm_sdk::emit_versioned_event!(env, (symbol_short!("upgraded"),), (new_wasm_hash,));
        Ok(())
    }

    fn extend_ttl(env: &Env) {
        env.storage().instance().extend_ttl(MIN_TTL, BUMP_TO);
    }

    fn read_admin(env: &Env) -> Result<Address, RouterError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(RouterError::NotInitialized)
    }

    /// Stored-admin check shared by `pause` / `unpause`: the caller-supplied
    /// address must equal the stored admin and must authorize the call.
    fn require_admin(env: &Env, admin: &Address) -> Result<(), RouterError> {
        let stored_admin = Self::read_admin(env)?;
        if *admin != stored_admin {
            return Err(RouterError::Unauthorized);
        }
        admin.require_auth();
        Ok(())
    }

    fn require_not_paused(env: &Env) -> Result<(), RouterError> {
        if Self::is_paused(env.clone()) {
            return Err(RouterError::Paused);
        }
        Ok(())
    }

    fn factory_client(env: &Env) -> Result<FactoryClient<'_>, RouterError> {
        let factory: Address = env
            .storage()
            .instance()
            .get(&DataKey::Factory)
            .ok_or(RouterError::NotInitialized)?;
        Ok(FactoryClient::new(env, &factory))
    }

    /// Shared precondition for every path-taking entry point.
    ///
    /// A repeated adjacent token resolves to a pool that cannot exist, which
    /// previously surfaced as a confusing factory panic; it is rejected here
    /// with a named message instead.
    fn require_valid_path(path: &Vec<Address>) -> Result<(), RouterError> {
        if path.len() < 2 {
            return Err(RouterError::InvalidPath);
        }
        let hops = path.len() - 1;
        for i in 0..hops {
            if path.get(i).unwrap() == path.get(i + 1).unwrap() {
                return Err(RouterError::DuplicateAdjacentToken);
            }
        }
        Ok(())
    }

    fn pool_for_hop(
        factory: &FactoryClient,
        path: &Vec<Address>,
        i: u32,
    ) -> Result<Address, RouterError> {
        let token_in = path.get(i).unwrap();
        let token_out = path.get(i + 1).unwrap();
        factory
            .get_pool(&token_in, &token_out)
            .ok_or(RouterError::PoolNotFound)
    }

    /// Walk `path` forwards, returning the amount held at each position.
    ///
    /// `result[0] == amount_in` and `result[i + 1]` is the output of hop `i`.
    fn forward_walk(
        env: &Env,
        factory: &FactoryClient,
        path: &Vec<Address>,
        amount_in: i128,
    ) -> Result<Vec<i128>, RouterError> {
        let hops = path.len() - 1;
        let mut amounts = Vec::new(env);
        amounts.push_back(amount_in);

        let mut current = amount_in;
        for i in 0..hops {
            let token_in = path.get(i).unwrap();
            let pool = Self::pool_for_hop(factory, path, i)?;
            current = AmmPoolClient::new(env, &pool).get_amount_out(&token_in, &current);
            amounts.push_back(current);
        }
        Ok(amounts)
    }

    /// Walk `path` backwards, returning the amount required at each position.
    ///
    /// `result[0]` is the input the route needs and the last element is
    /// `amount_out`. Shared by `swap_exact_out` and the exact-out quotes so the
    /// two can never disagree.
    fn reverse_walk(
        env: &Env,
        factory: &FactoryClient,
        path: &Vec<Address>,
        amount_out: i128,
    ) -> Result<Vec<i128>, RouterError> {
        let hops = path.len() - 1;
        let mut amounts = Vec::new(env);
        amounts.push_back(amount_out);

        let mut current_out = amount_out;
        for i in (0..hops).rev() {
            let token_out = path.get(i + 1).unwrap();
            let pool = Self::pool_for_hop(factory, path, i)?;
            let required_in =
                AmmPoolClient::new(env, &pool).get_amount_in(&token_out, &current_out);
            amounts.push_front(required_in);
            current_out = required_in;
        }
        Ok(amounts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use factory::{Factory, FactoryClient};
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::{
        testutils::{storage::Instance as _, Address as _, Ledger},
        Env,
    };

    fn setup_env_and_router() -> (Env, Address, Address, Address, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);

        env.budget().reset_unlimited();
        let amm_wasm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let lp_wasm_hash = env.deployer().upload_contract_wasm(token::WASM);

        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_wasm_hash, &lp_wasm_hash);

        let router_addr = env.register_contract(None, Router);
        let router = RouterClient::new(&env, &router_addr);
        router.initialize(&admin, &factory_addr);

        let token1 = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token2 = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token3 = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        // fee_tier 2 = 30 bps (Medium); governance_wasm_hash = None
        factory.create_pool(&admin, &token1, &token2, &2_i128, &None);
        factory.create_pool(&admin, &token2, &token3, &2_i128, &None);

        let pool1_addr = factory.get_pool(&token1, &token2).unwrap();
        let pool2_addr = factory.get_pool(&token2, &token3).unwrap();

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &token1).mint(&trader, &1_000_000_i128);
        StellarAssetClient::new(&env, &token2).mint(&trader, &1_000_000_i128);
        StellarAssetClient::new(&env, &token3).mint(&trader, &1_000_000_i128);

        let lp = Address::generate(&env);
        StellarAssetClient::new(&env, &token1).mint(&lp, &10_000_000_i128);
        StellarAssetClient::new(&env, &token2).mint(&lp, &10_000_000_i128);
        StellarAssetClient::new(&env, &token3).mint(&lp, &10_000_000_i128);

        amm::AmmPoolClient::new(&env, &pool1_addr).add_liquidity(
            &lp,
            &1_000_000,
            &1_000_000,
            &0,
            &u64::MAX,
        );
        amm::AmmPoolClient::new(&env, &pool2_addr).add_liquidity(
            &lp,
            &1_000_000,
            &1_000_000,
            &0,
            &u64::MAX,
        );

        (env, router_addr, trader, token1, token2, token3, pool1_addr)
    }

    #[test]
    fn test_expired_deadline() {
        let (env, router_addr, trader, token1, token2, token3, _) = setup_env_and_router();
        env.ledger().with_mut(|li| {
            li.timestamp = 1000;
        });

        let router = RouterClient::new(&env, &router_addr);
        let path = soroban_sdk::vec![&env, token1.clone(), token2.clone(), token3.clone()];
        let result = router.try_swap_exact_in(&trader, &path, &100_000, &0, &500);
        assert_eq!(result, Err(Ok(RouterError::DeadlineExceeded)));
    }

    #[test]
    fn test_slippage_exceeded() {
        let (env, router_addr, trader, token1, token2, token3, _) = setup_env_and_router();

        let router = RouterClient::new(&env, &router_addr);
        let path = soroban_sdk::vec![&env, token1.clone(), token2.clone(), token3.clone()];
        // The final hop carries `min_amount_out` down to the pool, so an
        // unreachable floor trips the pool's own slippage guard first and
        // surfaces as a sub-call host error, not a `RouterError`.
        let result = router.try_swap_exact_in(&trader, &path, &100_000, &1_000_000_000, &u64::MAX);
        assert!(result.is_err());
    }

    #[test]
    fn test_successful_route_execution() {
        let (env, router_addr, trader, token1, token2, token3, _) = setup_env_and_router();

        let router = RouterClient::new(&env, &router_addr);
        let path = soroban_sdk::vec![&env, token1.clone(), token2.clone(), token3.clone()];

        let out = router.swap_exact_in(&trader, &path, &10_000, &0, &u64::MAX);
        assert!(out > 0);
    }

    #[test]
    fn test_atomic_revert_behavior() {
        let (env, router_addr, trader, token1, token2, token3, _pool1_addr) =
            setup_env_and_router();

        let router = RouterClient::new(&env, &router_addr);
        let path = soroban_sdk::vec![&env, token1.clone(), token2.clone(), token3.clone()];

        // This fails, so the state must be reverted. In actual Soroban the
        // whole invocation reverts atomically on error.
        let result = router.try_swap_exact_in(&trader, &path, &10_000, &1_000_000, &u64::MAX);
        assert!(result.is_err());
    }

    /// Everything `setup_env_and_router` builds, plus a fourth token and a
    /// `token3 <-> token4` pool so three-hop routes can be exercised, and an
    /// `orphan` token that deliberately has no pool at all.
    struct ThreeHop {
        env: Env,
        router: Address,
        trader: Address,
        token1: Address,
        token2: Address,
        token3: Address,
        token4: Address,
        orphan: Address,
        factory: Address,
        admin: Address,
    }

    fn setup_three_hop() -> ThreeHop {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);

        env.budget().reset_unlimited();
        let amm_wasm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let lp_wasm_hash = env.deployer().upload_contract_wasm(token::WASM);

        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_wasm_hash, &lp_wasm_hash);

        let router_addr = env.register_contract(None, Router);
        RouterClient::new(&env, &router_addr).initialize(&admin, &factory_addr);

        let mut tokens: soroban_sdk::Vec<Address> = soroban_sdk::Vec::new(&env);
        for _ in 0..4 {
            tokens.push_back(
                env.register_stellar_asset_contract_v2(admin.clone())
                    .address(),
            );
        }
        let orphan = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let lp = Address::generate(&env);
        let trader = Address::generate(&env);
        for i in 0..tokens.len() {
            let t = tokens.get(i).unwrap();
            StellarAssetClient::new(&env, &t).mint(&lp, &10_000_000_i128);
            StellarAssetClient::new(&env, &t).mint(&trader, &1_000_000_i128);
        }

        // fee_tier 2 = 30 bps (Medium); governance_wasm_hash = None
        for i in 0..3 {
            let a = tokens.get(i).unwrap();
            let b = tokens.get(i + 1).unwrap();
            factory.create_pool(&admin, &a, &b, &2_i128, &None);
            let pool = factory.get_pool(&a, &b).unwrap();
            amm::AmmPoolClient::new(&env, &pool).add_liquidity(
                &lp,
                &1_000_000,
                &1_000_000,
                &0,
                &u64::MAX,
            );
        }

        ThreeHop {
            env,
            router: router_addr,
            trader,
            token1: tokens.get(0).unwrap(),
            token2: tokens.get(1).unwrap(),
            token3: tokens.get(2).unwrap(),
            token4: tokens.get(3).unwrap(),
            orphan,
            factory: factory_addr,
            admin,
        }
    }

    // -- #683: exact-out quoting, per-hop breakdowns, path validation ---------

    #[test]
    fn test_single_hop_quotes_agree_with_pool() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let path = soroban_sdk::vec![&s.env, s.token1.clone(), s.token2.clone()];

        let pool = FactoryClient::new(&s.env, &s.factory)
            .get_pool(&s.token1, &s.token2)
            .unwrap();
        let pool_client = amm::AmmPoolClient::new(&s.env, &pool);

        assert_eq!(
            router.get_amount_out_path(&path, &10_000),
            pool_client.get_amount_out(&s.token1, &10_000)
        );
        assert_eq!(
            router.get_amount_in_path(&path, &10_000),
            pool_client.get_amount_in(&s.token2, &10_000)
        );
    }

    #[test]
    fn test_amounts_out_path_matches_scalar_quote_for_two_and_three_hops() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);

        let two_hop = soroban_sdk::vec![&s.env, s.token1.clone(), s.token2.clone()];
        let amounts = router.get_amounts_out_path(&two_hop, &10_000);
        assert_eq!(amounts.len(), 2);
        assert_eq!(amounts.get(0).unwrap(), 10_000);
        assert_eq!(
            amounts.last().unwrap(),
            router.get_amount_out_path(&two_hop, &10_000)
        );

        let three_hop = soroban_sdk::vec![
            &s.env,
            s.token1.clone(),
            s.token2.clone(),
            s.token3.clone(),
            s.token4.clone()
        ];
        let amounts = router.get_amounts_out_path(&three_hop, &10_000);
        assert_eq!(amounts.len(), 4);
        assert_eq!(amounts.get(0).unwrap(), 10_000);
        assert_eq!(
            amounts.last().unwrap(),
            router.get_amount_out_path(&three_hop, &10_000)
        );
        // Each hop takes a fee, so the route strictly decreases.
        for i in 0..3u32 {
            assert!(amounts.get(i + 1).unwrap() < amounts.get(i).unwrap());
        }
    }

    #[test]
    fn test_amounts_in_path_breakdown_ends_at_requested_output() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let path = soroban_sdk::vec![&s.env, s.token1.clone(), s.token2.clone(), s.token3.clone()];

        let amounts = router.get_amounts_in_path(&path, &10_000);
        assert_eq!(amounts.len(), 3);
        assert_eq!(amounts.last().unwrap(), 10_000);
        assert_eq!(
            amounts.get(0).unwrap(),
            router.get_amount_in_path(&path, &10_000)
        );
        // Walking backwards, each earlier position needs strictly more.
        assert!(amounts.get(0).unwrap() > amounts.get(1).unwrap());
        assert!(amounts.get(1).unwrap() > amounts.get(2).unwrap());
    }

    #[test]
    fn test_in_out_round_trip_never_exceeds_the_original_input() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);

        for path in [
            soroban_sdk::vec![&s.env, s.token1.clone(), s.token2.clone()],
            soroban_sdk::vec![
                &s.env,
                s.token1.clone(),
                s.token2.clone(),
                s.token3.clone(),
                s.token4.clone()
            ],
        ] {
            let hops = (path.len() - 1) as i128;
            let amount_in = 10_000_i128;
            let out = router.get_amount_out_path(&path, &amount_in);
            let round_trip = router.get_amount_in_path(&path, &out);

            // Quoting back the exact output can never ask for more than the
            // input that produced it, and integer rounding costs at most one
            // unit per hop.
            assert!(round_trip <= amount_in);
            assert!(amount_in - round_trip <= hops);
        }
    }

    #[test]
    fn test_get_pools_for_path_resolves_every_hop() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let factory = FactoryClient::new(&s.env, &s.factory);
        let path = soroban_sdk::vec![&s.env, s.token1.clone(), s.token2.clone(), s.token3.clone()];

        let pools = router.get_pools_for_path(&path);
        assert_eq!(pools.len(), 2);
        assert_eq!(
            pools.get(0).unwrap(),
            factory.get_pool(&s.token1, &s.token2).unwrap()
        );
        assert_eq!(
            pools.get(1).unwrap(),
            factory.get_pool(&s.token2, &s.token3).unwrap()
        );
    }

    #[test]
    fn test_get_pools_for_path_reports_missing_pool() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let path = soroban_sdk::vec![&s.env, s.token1.clone(), s.token2.clone(), s.orphan.clone()];
        let result = router.try_get_pools_for_path(&path);
        assert_eq!(result, Err(Ok(RouterError::PoolNotFound)));
    }

    #[test]
    fn test_is_path_routable_distinguishes_missing_pools() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);

        let good = soroban_sdk::vec![&s.env, s.token1.clone(), s.token2.clone(), s.token3.clone()];
        assert!(router.is_path_routable(&good));

        let missing = soroban_sdk::vec![&s.env, s.token1.clone(), s.orphan.clone()];
        assert!(!router.is_path_routable(&missing));
        // The ambiguous legacy sentinel for exactly the same path.
        assert_eq!(router.get_amount_out_path(&missing, &10_000), 0);

        // Malformed paths are reported, not panicked on.
        assert!(!router.is_path_routable(&soroban_sdk::Vec::new(&s.env)));
        assert!(!router.is_path_routable(&soroban_sdk::vec![&s.env, s.token1.clone()]));
        assert!(!router.is_path_routable(&soroban_sdk::vec![
            &s.env,
            s.token1.clone(),
            s.token1.clone()
        ]));
    }

    #[test]
    fn test_duplicate_adjacent_token_is_rejected_by_name() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let path = soroban_sdk::vec![&s.env, s.token1.clone(), s.token2.clone(), s.token2.clone()];
        let result = router.try_get_amount_out_path(&path, &10_000);
        assert_eq!(result, Err(Ok(RouterError::DuplicateAdjacentToken)));
    }

    #[test]
    fn test_duplicate_adjacent_token_is_rejected_on_exact_out_quote() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let path = soroban_sdk::vec![&s.env, s.token1.clone(), s.token1.clone()];
        let result = router.try_get_amount_in_path(&path, &10_000);
        assert_eq!(result, Err(Ok(RouterError::DuplicateAdjacentToken)));
    }

    #[test]
    fn test_empty_path_is_rejected() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let result = router.try_get_amounts_out_path(&soroban_sdk::Vec::new(&s.env), &10_000);
        assert_eq!(result, Err(Ok(RouterError::InvalidPath)));
    }

    #[test]
    fn test_single_element_path_is_rejected() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let path = soroban_sdk::vec![&s.env, s.token1.clone()];
        let result = router.try_get_amounts_in_path(&path, &10_000);
        assert_eq!(result, Err(Ok(RouterError::InvalidPath)));
    }

    #[test]
    fn test_zero_amount_in_is_rejected() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let path = soroban_sdk::vec![&s.env, s.token1.clone(), s.token2.clone()];
        let result = router.try_get_amounts_out_path(&path, &0);
        assert_eq!(result, Err(Ok(RouterError::InvalidAmount)));
    }

    #[test]
    fn test_zero_amount_out_is_rejected() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let path = soroban_sdk::vec![&s.env, s.token1.clone(), s.token2.clone()];
        let result = router.try_get_amount_in_path(&path, &0);
        assert_eq!(result, Err(Ok(RouterError::InvalidAmount)));
    }

    #[test]
    fn test_swap_exact_out_still_matches_its_quote_after_the_refactor() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let path = soroban_sdk::vec![&s.env, s.token1.clone(), s.token2.clone(), s.token3.clone()];

        let quoted_in = router.get_amount_in_path(&path, &10_000);
        let spent = router.swap_exact_out(&s.trader, &path, &10_000, &quoted_in, &u64::MAX);
        assert_eq!(spent, quoted_in);
    }

    #[test]
    fn test_swap_exact_out_enforces_max_in() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let path = soroban_sdk::vec![&s.env, s.token1.clone(), s.token2.clone()];

        let quoted_in = router.get_amount_in_path(&path, &10_000);
        let result =
            router.try_swap_exact_out(&s.trader, &path, &10_000, &(quoted_in - 1), &u64::MAX);
        assert_eq!(result, Err(Ok(RouterError::SlippageExceeded)));
    }

    // -- #926: typed error enum -----------------------------------------------

    #[test]
    fn test_double_initialize_is_rejected() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let admin = Address::generate(&s.env);
        let result = router.try_initialize(&admin, &s.factory);
        assert_eq!(result, Err(Ok(RouterError::AlreadyInitialized)));
    }

    #[test]
    fn test_calls_before_initialize_report_not_initialized() {
        let env = Env::default();
        env.mock_all_auths();
        let router_addr = env.register_contract(None, Router);
        let router = RouterClient::new(&env, &router_addr);

        assert_eq!(
            router.try_get_factory(),
            Err(Ok(RouterError::NotInitialized))
        );

        let t1 = Address::generate(&env);
        let t2 = Address::generate(&env);
        let path = soroban_sdk::vec![&env, t1, t2];
        assert_eq!(
            router.try_get_amount_out_path(&path, &10_000),
            Err(Ok(RouterError::NotInitialized))
        );
    }

    #[test]
    fn test_swap_exact_in_reports_missing_pool() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        // token1 -> orphan has no deployed pool.
        let path = soroban_sdk::vec![&s.env, s.token1.clone(), s.orphan.clone()];
        let result = router.try_swap_exact_in(&s.trader, &path, &10_000, &0, &u64::MAX);
        assert_eq!(result, Err(Ok(RouterError::PoolNotFound)));
    }

    #[test]
    fn test_swap_exact_in_rejects_zero_amount() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let path = soroban_sdk::vec![&s.env, s.token1.clone(), s.token2.clone()];
        let result = router.try_swap_exact_in(&s.trader, &path, &0, &0, &u64::MAX);
        assert_eq!(result, Err(Ok(RouterError::InvalidAmount)));
    }

    // -- #937: pause / unpause safety switch ----------------------------------

    /// Payload of the last event the router emitted under `topic`, with the
    /// schema-version prefix asserted and stripped.
    fn last_router_payload(env: &Env, router: &Address, topic: soroban_sdk::Symbol) -> (Address,) {
        use soroban_sdk::{testutils::Events as _, IntoVal};
        let event = env
            .events()
            .all()
            .iter()
            .rfind(|e| e.0 == *router && e.1 == (topic.clone(),).into_val(env))
            .unwrap_or_else(|| panic!("no event emitted for the requested topic"));
        let (version, payload): (u32, (Address,)) = event.2.into_val(env);
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        payload
    }

    #[test]
    fn test_pause_and_unpause_by_stored_admin() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        assert!(!router.is_paused());

        router.pause(&s.admin);
        // The stored admin is the address that had to authorize the call.
        let auths = s.env.auths();
        assert_eq!(auths.len(), 1);
        assert_eq!(auths[0].0, s.admin);
        assert!(router.is_paused());
        assert_eq!(
            last_router_payload(&s.env, &s.router, symbol_short!("pause")),
            (s.admin.clone(),)
        );

        router.unpause(&s.admin);
        assert!(!router.is_paused());
        assert_eq!(
            last_router_payload(&s.env, &s.router, symbol_short!("unpause")),
            (s.admin.clone(),)
        );
    }

    #[test]
    fn test_pause_and_unpause_reject_non_admin_address() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let intruder = Address::generate(&s.env);

        assert_eq!(
            router.try_pause(&intruder),
            Err(Ok(RouterError::Unauthorized))
        );
        assert!(!router.is_paused());

        router.pause(&s.admin);
        assert_eq!(
            router.try_unpause(&intruder),
            Err(Ok(RouterError::Unauthorized))
        );
        assert!(router.is_paused());
    }

    #[test]
    fn test_pause_and_unpause_require_admin_authorization() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);

        // Passing the right address is not enough; the admin must sign.
        s.env.mock_auths(&[]);
        assert!(router.try_pause(&s.admin).is_err());
        assert!(!router.is_paused());

        s.env.mock_all_auths();
        router.pause(&s.admin);
        s.env.mock_auths(&[]);
        assert!(router.try_unpause(&s.admin).is_err());
        assert!(router.is_paused());
    }

    #[test]
    fn test_pause_before_initialize_returns_not_initialized() {
        let env = Env::default();
        env.mock_all_auths();
        let router_addr = env.register_contract(None, Router);
        let router = RouterClient::new(&env, &router_addr);
        let admin = Address::generate(&env);

        assert_eq!(
            router.try_pause(&admin),
            Err(Ok(RouterError::NotInitialized))
        );
        assert_eq!(
            router.try_unpause(&admin),
            Err(Ok(RouterError::NotInitialized))
        );
        assert!(!router.is_paused());
    }

    #[test]
    fn test_every_swap_entrypoint_rejects_while_paused() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let path = soroban_sdk::vec![&s.env, s.token1.clone(), s.token2.clone(), s.token3.clone()];
        router.pause(&s.admin);

        assert_eq!(
            router.try_swap_exact_in(&s.trader, &path, &10_000, &0, &u64::MAX),
            Err(Ok(RouterError::Paused))
        );
        assert_eq!(
            router.try_swap_exact_out(&s.trader, &path, &10_000, &i128::MAX, &u64::MAX),
            Err(Ok(RouterError::Paused))
        );

        // Unpausing restores both swap paths.
        router.unpause(&s.admin);
        assert!(router.swap_exact_in(&s.trader, &path, &10_000, &0, &u64::MAX) > 0);
        assert!(router.swap_exact_out(&s.trader, &path, &10_000, &i128::MAX, &u64::MAX) > 0);
    }

    #[test]
    fn test_read_only_views_callable_while_paused() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let path = soroban_sdk::vec![&s.env, s.token1.clone(), s.token2.clone(), s.token3.clone()];
        router.pause(&s.admin);

        assert!(router.is_paused());
        assert_eq!(router.get_factory(), s.factory);
        assert!(router.is_path_routable(&path));
        assert!(router.get_amount_out_path(&path, &10_000) > 0);
        assert!(router.get_amount_in_path(&path, &10_000) > 0);
        assert_eq!(router.get_amounts_out_path(&path, &10_000).len(), 3);
        assert_eq!(router.get_amounts_in_path(&path, &10_000).len(), 3);
        assert_eq!(router.get_pools_for_path(&path).len(), 2);
    }

    #[test]
    fn test_admin_rotation_replaces_pause_authority() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let next = Address::generate(&s.env);
        router.propose_admin(&s.admin, &next);
        assert_eq!(router.get_pending_admin(), Some(next.clone()));
        router.accept_admin(&next);
        assert_eq!(router.get_admin(), next.clone());
        assert_eq!(
            router.try_pause(&s.admin),
            Err(Ok(RouterError::Unauthorized))
        );
        router.pause(&next);
    }

    #[test]
    fn test_admin_rotation_rejects_unauthorized_proposal() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        assert_eq!(
            router.try_propose_admin(&Address::generate(&s.env), &Address::generate(&s.env)),
            Err(Ok(RouterError::Unauthorized))
        );
    }

    #[test]
    fn test_admin_rotation_rejects_stranger_and_missing_nomination() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let nominee = Address::generate(&s.env);
        assert_eq!(
            router.try_accept_admin(&nominee),
            Err(Ok(RouterError::NoPendingAdmin))
        );
        router.propose_admin(&s.admin, &nominee);
        assert_eq!(
            router.try_accept_admin(&Address::generate(&s.env)),
            Err(Ok(RouterError::Unauthorized))
        );
    }

    #[test]
    fn test_upgrade_requires_admin_and_preserves_router_state() {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let router_wasm = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/wasm32v1-none/release/router.wasm"
        ));
        let router_wasm = soroban_sdk::Bytes::from_slice(&s.env, router_wasm);
        let wasm_hash = s.env.deployer().upload_contract_wasm(router_wasm);
        assert_eq!(
            router.try_upgrade(&Address::generate(&s.env), &wasm_hash),
            Err(Ok(RouterError::Unauthorized))
        );
        router.upgrade(&s.admin, &wasm_hash);
        assert_eq!(router.get_factory(), s.factory);
        assert_eq!(router.get_admin(), s.admin);
    }

    #[test]
    fn test_initialize_rejects_non_factory_without_writing_state() {
        let env = Env::default();
        env.mock_all_auths();
        let router_addr = env.register_contract(None, Router);
        let client = RouterClient::new(&env, &router_addr);
        let admin = Address::generate(&env);
        let not_factory = Address::generate(&env);
        assert_eq!(
            client.try_initialize(&admin, &not_factory),
            Err(Ok(RouterError::InvalidFactory))
        );
        assert_eq!(
            client.try_get_factory(),
            Err(Ok(RouterError::NotInitialized))
        );
    }

    fn last_route_event(env: &Env, router: &Address) -> (Address, Vec<Address>, i128, i128, u32) {
        use soroban_sdk::{testutils::Events as _, IntoVal};
        let event = env
            .events()
            .all()
            .iter()
            .rfind(|e| e.0 == *router && e.1 == (symbol_short!("route"),).into_val(env))
            .unwrap();
        let (version, payload): (u32, (Address, Vec<Address>, i128, i128, u32)) =
            event.2.into_val(env);
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        payload
    }

    #[test]
    fn test_swap_exact_in_emits_one_complete_route_event() {
        use soroban_sdk::{testutils::Events as _, IntoVal};
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let path = soroban_sdk::vec![
            &s.env,
            s.token1.clone(),
            s.token2.clone(),
            s.token3.clone(),
            s.token4.clone()
        ];
        let amount_out = router.swap_exact_in(&s.trader, &path, &10_000, &0, &u64::MAX);
        let routes = s
            .env
            .events()
            .all()
            .iter()
            .filter(|e| e.0 == s.router && e.1 == (symbol_short!("route"),).into_val(&s.env))
            .count();
        assert_eq!(routes, 1);
        assert_eq!(
            last_route_event(&s.env, &s.router),
            (s.trader, path, 10_000, amount_out, 3)
        );
    }

    #[test]
    fn test_swap_exact_out_emits_actual_input_and_requested_output() {
        use soroban_sdk::{testutils::Events as _, IntoVal};
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let path = soroban_sdk::vec![
            &s.env,
            s.token1.clone(),
            s.token2.clone(),
            s.token3.clone(),
            s.token4.clone()
        ];
        let amount_in = router.swap_exact_out(&s.trader, &path, &10_000, &i128::MAX, &u64::MAX);
        let routes = s
            .env
            .events()
            .all()
            .iter()
            .filter(|e| e.0 == s.router && e.1 == (symbol_short!("route"),).into_val(&s.env))
            .count();
        assert_eq!(routes, 1);
        assert_eq!(
            last_route_event(&s.env, &s.router),
            (s.trader, path, amount_in, 10_000, 3)
        );
    }

    fn assert_view_ttl_bump(view: u32) {
        let s = setup_three_hop();
        let router = RouterClient::new(&s.env, &s.router);
        let path = soroban_sdk::vec![&s.env, s.token1.clone(), s.token2.clone(), s.token3.clone()];
        s.env.ledger().set_sequence_number(BUMP_TO - MIN_TTL + 1);
        match view {
            0 => {
                let _ = router.get_amount_in_path(&path, &10_000);
            }
            1 => {
                let _ = router.get_amounts_out_path(&path, &10_000);
            }
            2 => {
                let _ = router.get_amounts_in_path(&path, &10_000);
            }
            3 => {
                let _ = router.get_pools_for_path(&path);
            }
            4 => {
                let _ = router.is_path_routable(&path);
            }
            5 => {
                let _ = router.get_factory();
            }
            _ => {
                let _ = router.is_paused();
            }
        }
        assert_eq!(
            s.env
                .as_contract(&s.router, || s.env.storage().instance().get_ttl()),
            BUMP_TO
        );
    }

    #[test]
    fn test_ttl_bump_get_amount_in_path() {
        assert_view_ttl_bump(0);
    }
    #[test]
    fn test_ttl_bump_get_amounts_out_path() {
        assert_view_ttl_bump(1);
    }
    #[test]
    fn test_ttl_bump_get_amounts_in_path() {
        assert_view_ttl_bump(2);
    }
    #[test]
    fn test_ttl_bump_get_pools_for_path() {
        assert_view_ttl_bump(3);
    }
    #[test]
    fn test_ttl_bump_is_path_routable() {
        assert_view_ttl_bump(4);
    }
    #[test]
    fn test_ttl_bump_get_factory() {
        assert_view_ttl_bump(5);
    }
    #[test]
    fn test_ttl_bump_is_paused() {
        assert_view_ttl_bump(6);
    }
}
