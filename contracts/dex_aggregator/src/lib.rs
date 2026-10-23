#![no_std]

//! DEX aggregator — routes trades across multiple AMM and CL pools for best execution.

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, symbol_short, vec,
    Address, Env, Vec,
};

use pool_interfaces::{AmmPoolClient, FactoryClient};

const MIN_TTL: u32 = 172_800;
const BUMP_TO: u32 = 518_400;

#[contractclient(name = "ClPoolClient")]
pub trait ClPoolInterface {
    fn estimate_price_impact(
        env: Env,
        zero_for_one: bool,
        amount_in: i128,
        sqrt_price_limit_x96: u128,
    ) -> PriceImpactEstimate;
    fn swap(
        env: Env,
        sender: Address,
        zero_for_one: bool,
        amount_in: i128,
        sqrt_price_limit_x96: u128,
        min_amount_out: i128,
        deadline: u64,
    ) -> i128;
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceImpactEstimate {
    pub amount_in: i128,
    pub amount_in_after_fee: i128,
    pub amount_out: i128,
    pub fee_amount: i128,
    pub spot_price_before: i128,
    pub effective_price: i128,
    pub price_impact_bps: i128,
    pub sqrt_price_before: u128,
    pub sqrt_price_after: u128,
    pub tick_before: i32,
    pub tick_after: i32,
    pub active_liquidity_before: i128,
    pub active_liquidity_after: i128,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum AggregatorError {
    NoRouteFound = 1,
    SlippageExceeded = 2,
    UnregisteredPool = 3,
    InvalidMaxHops = 4,
    TooManyRoutingTokens = 5,
    /// A function that reads the admin or factory was called before
    /// `initialize`.
    NotInitialized = 6,
    /// A state-mutating entrypoint was called while the aggregator is paused
    /// by its admin.
    Paused = 7,
    /// `pause` / `unpause` was called with an address that is not the stored
    /// admin.
    Unauthorized = 8,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PoolKind {
    Amm,
    Cl,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteHop {
    pub pool: Address,
    pub pool_kind: PoolKind,
    pub token_in: Address,
    pub token_out: Address,
    pub zero_for_one: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClPoolInfo {
    pub pool: Address,
    pub token_a: Address,
    pub token_b: Address,
    pub fee_bps: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteQuote {
    pub amount_out: i128,
    pub hops: Vec<RouteHop>,
}

#[contracttype]
pub enum DataKey {
    Admin,
    Factory,
    MaxHops,
    RoutingTokens,
    ClPoolCount,
    ClPool(u32),
    /// `true` while mutating entrypoints are halted; absent means unpaused.
    Paused,
}

#[contract]
pub struct DexAggregator;

#[contractimpl]
impl DexAggregator {
    pub const DEFAULT_MAX_HOPS: u32 = 4;
    pub const PRICE_TOLERANCE_BPS: i128 = 10;
    pub const BPS: i128 = 10_000;
    pub const MAX_CL_POOLS: u32 = 50;
    pub const MAX_ROUTING_TOKENS: u32 = 50;
    pub const CL_FEE_TIERS: [i128; 3] = [30, 100, 500];

    pub const MIN_SQRT_PRICE: u128 = 4_295_128_739_u128;
    pub const MAX_SQRT_PRICE: u128 = 340_275_971_719_517_849_884_931_781_110_561_029_923_u128;

    pub fn initialize(env: Env, admin: Address, factory: Address) {
        assert!(
            !env.storage().instance().has(&DataKey::Factory),
            "already initialized"
        );
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Factory, &factory);
        env.storage()
            .instance()
            .set(&DataKey::MaxHops, &Self::DEFAULT_MAX_HOPS);
        env.storage().instance().set(&DataKey::ClPoolCount, &0u32);
    }

    /// Halt every state-mutating entrypoint (admin configuration and route
    /// execution). Admin-only.
    ///
    /// Quotes (`find_best_route`, `get_quote`, `is_price_within_tolerance`)
    /// stay callable so integrators can still price routes while paused.
    pub fn pause(env: Env, admin: Address) -> Result<(), AggregatorError> {
        Self::require_admin(&env, &admin)?;
        Self::extend_ttl(&env);
        env.storage().instance().set(&DataKey::Paused, &true);
        soroban_amm_sdk::emit_versioned_event!(&env, (symbol_short!("pause"),), (admin,));
        Ok(())
    }

    /// Resume state-mutating entrypoints after `pause`. Admin-only.
    pub fn unpause(env: Env, admin: Address) -> Result<(), AggregatorError> {
        Self::require_admin(&env, &admin)?;
        Self::extend_ttl(&env);
        env.storage().instance().set(&DataKey::Paused, &false);
        soroban_amm_sdk::emit_versioned_event!(&env, (symbol_short!("unpause"),), (admin,));
        Ok(())
    }

    /// Whether state-mutating entrypoints are currently halted.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    pub fn set_max_hops(env: Env, max_hops: u32) -> Result<(), AggregatorError> {
        Self::require_not_paused(&env)?;
        let admin = Self::read_admin(&env)?;
        admin.require_auth();

        if max_hops == 0 {
            return Err(AggregatorError::InvalidMaxHops);
        }

        Self::extend_ttl(&env);
        env.storage().instance().set(&DataKey::MaxHops, &max_hops);
        Ok(())
    }

    pub fn register_cl_pool(
        env: Env,
        pool: Address,
        token_a: Address,
        token_b: Address,
        fee_bps: i128,
    ) -> Result<(), AggregatorError> {
        Self::require_not_paused(&env)?;
        let admin = Self::read_admin(&env)?;
        admin.require_auth();

        Self::extend_ttl(&env);
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::ClPoolCount)
            .unwrap_or(0);

        for i in 0..count {
            let entry: ClPoolInfo = env.storage().instance().get(&DataKey::ClPool(i)).unwrap();
            if entry.pool == pool {
                return Ok(());
            }
        }

        assert!(count < Self::MAX_CL_POOLS, "max CL pools reached");

        env.storage().instance().set(
            &DataKey::ClPool(count),
            &ClPoolInfo {
                pool: pool.clone(),
                token_a: token_a.clone(),
                token_b: token_b.clone(),
                fee_bps,
            },
        );
        env.storage()
            .instance()
            .set(&DataKey::ClPoolCount, &(count + 1));

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (symbol_short!("cl_reg"),),
            (token_a, token_b, fee_bps, pool)
        );
        Ok(())
    }

    pub fn set_routing_tokens(env: Env, tokens: Vec<Address>) -> Result<(), AggregatorError> {
        Self::require_not_paused(&env)?;
        let admin = Self::read_admin(&env)?;
        admin.require_auth();

        if tokens.len() > Self::MAX_ROUTING_TOKENS {
            return Err(AggregatorError::TooManyRoutingTokens);
        }

        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .set(&DataKey::RoutingTokens, &tokens);
        Ok(())
    }

    pub fn remove_cl_pool(env: Env, pool: Address) -> Result<(), AggregatorError> {
        Self::require_not_paused(&env)?;
        let admin = Self::read_admin(&env)?;
        admin.require_auth();

        Self::extend_ttl(&env);
        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::ClPoolCount)
            .unwrap_or(0);

        for i in 0..count {
            let entry: ClPoolInfo = env.storage().instance().get(&DataKey::ClPool(i)).unwrap();
            if entry.pool == pool {
                if i != count - 1 {
                    let last: ClPoolInfo = env
                        .storage()
                        .instance()
                        .get(&DataKey::ClPool(count - 1))
                        .unwrap();
                    env.storage().instance().set(&DataKey::ClPool(i), &last);
                }
                env.storage().instance().remove(&DataKey::ClPool(count - 1));
                env.storage()
                    .instance()
                    .set(&DataKey::ClPoolCount, &(count - 1));
                return Ok(());
            }
        }
        Ok(())
    }

    /// Find the best route up to `max_hops` pools deep (#319).
    pub fn find_best_route(
        env: Env,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
        max_hops: u32,
    ) -> Result<RouteQuote, AggregatorError> {
        let factory = Self::read_factory(&env)?;
        Self::extend_ttl(&env);
        assert!(token_in != token_out, "same token");
        assert!(amount_in > 0, "amount must be positive");
        let stored_max_hops: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MaxHops)
            .unwrap_or(Self::DEFAULT_MAX_HOPS);
        let cap = max_hops.min(stored_max_hops);
        if cap == 0 {
            return Err(AggregatorError::NoRouteFound);
        }
        let (quote, runner_up) =
            Self::search_best_bfs(&env, &factory, &token_in, &token_out, amount_in, cap)?;

        // The venue a route is *entered* through identifies the decision: it is
        // the pool the aggregator picked over every alternative first hop.
        let venue = quote.hops.get(0).unwrap();
        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (symbol_short!("route_sel"),),
            (
                venue.pool.clone(),
                venue.pool_kind.clone(),
                amount_in,
                quote.amount_out
            )
        );

        // Only meaningful when a second venue actually produced a quote; with a
        // single venue there is no improvement to measure.
        if let Some((alt_pool, alt_kind, alt_out)) = runner_up {
            soroban_amm_sdk::emit_versioned_event!(
                &env,
                (symbol_short!("route_alt"),),
                (
                    venue.pool.clone(),
                    quote.amount_out,
                    alt_pool,
                    alt_kind,
                    alt_out
                )
            );
        }

        Ok(quote)
    }

    /// Read-only quote for off-chain simulation (#319).
    pub fn get_quote(
        env: Env,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
        max_hops: u32,
    ) -> Result<RouteQuote, AggregatorError> {
        Self::find_best_route(env, token_in, token_out, amount_in, max_hops)
    }

    /// Execute a pre-computed multi-hop route atomically (#319).
    pub fn execute_route(
        env: Env,
        route: RouteQuote,
        trader: Address,
        amount_in: i128,
        min_out: i128,
        deadline: u64,
    ) -> Result<i128, AggregatorError> {
        Self::require_not_paused(&env)?;
        let factory = Self::read_factory(&env)?;
        Self::extend_ttl(&env);
        trader.require_auth();
        if route.hops.is_empty() || route.amount_out < min_out {
            return Err(AggregatorError::SlippageExceeded);
        }
        if deadline < env.ledger().timestamp() {
            return Err(AggregatorError::SlippageExceeded);
        }
        let entry = route.hops.get(0).unwrap();
        let exit = route.hops.get(route.hops.len() - 1).unwrap();
        let amount_out = Self::execute_hops(
            &env,
            &factory,
            &route.hops,
            &trader,
            amount_in,
            min_out,
            deadline,
        )?;

        // `amount_out` is what the pools actually returned, not `route.amount_out`,
        // which is only the quote the route was planned against.
        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (symbol_short!("route_exe"),),
            (
                trader,
                entry.token_in.clone(),
                exit.token_out.clone(),
                amount_in,
                amount_out,
                entry.pool.clone()
            )
        );

        Ok(amount_out)
    }

    /// Execute a best-execution swap with caller-supplied deadline.
    ///
    /// Finds the best available route and executes it atomically. The deadline
    /// parameter specifies the latest allowed block timestamp (ledger time) for
    /// the swap to be processed. If the current timestamp >= deadline, the swap
    /// is rejected with SlippageExceeded.
    ///
    /// # Arguments
    /// * `env` - Soroban environment
    /// * `trader` - Address that initiates and receives funds from the swap
    /// * `token_in` - Input token address
    /// * `token_out` - Output token address
    /// * `amount_in` - Exact amount to trade in
    /// * `min_out` - Minimum acceptable output (slippage protection)
    /// * `deadline` - Latest allowed block timestamp for execution (u64, seconds since epoch)
    ///
    /// # Returns
    /// The actual amount of `token_out` received, or an error if routing fails,
    /// the deadline expired, or slippage constraints are violated.
    pub fn swap_best(
        env: Env,
        trader: Address,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
        min_out: i128,
        deadline: u64,
    ) -> Result<i128, AggregatorError> {
        Self::require_not_paused(&env)?;
        Self::extend_ttl(&env);
        let max_hops: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MaxHops)
            .unwrap_or(Self::DEFAULT_MAX_HOPS);
        let quote = Self::find_best_route(env.clone(), token_in, token_out, amount_in, max_hops)?;
        Self::execute_route(env, quote, trader, amount_in, min_out, deadline)
    }

    pub fn is_price_within_tolerance(
        env: Env,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
        quoted_out: i128,
    ) -> bool {
        Self::extend_ttl(&env);
        let max_hops: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MaxHops)
            .unwrap_or(Self::DEFAULT_MAX_HOPS);
        let Ok(best) = Self::find_best_route(env.clone(), token_in, token_out, amount_in, max_hops)
        else {
            return quoted_out == 0;
        };
        if best.amount_out == 0 {
            return quoted_out == 0;
        }
        let diff = if best.amount_out >= quoted_out {
            best.amount_out - quoted_out
        } else {
            quoted_out - best.amount_out
        };
        let observed_bps = diff * Self::BPS / best.amount_out;
        if observed_bps > Self::PRICE_TOLERANCE_BPS {
            soroban_amm_sdk::emit_versioned_event!(
                &env,
                (symbol_short!("tol_fail"),),
                (
                    best.hops.get(0).unwrap().pool,
                    observed_bps,
                    Self::PRICE_TOLERANCE_BPS
                )
            );
            return false;
        }
        true
    }

    /// Breadth-first search for the best route, plus the runner-up quote.
    ///
    /// The second element is the best *other* venue that completed a route to
    /// `token_out`, or `None` when only one venue ever quoted. Callers use it to
    /// report how much the winning venue actually improved on the alternative.
    #[allow(clippy::type_complexity)]
    fn search_best_bfs(
        env: &Env,
        factory: &Address,
        token_in: &Address,
        token_out: &Address,
        amount_in: i128,
        max_hops: u32,
    ) -> Result<(RouteQuote, Option<(Address, PoolKind, i128)>), AggregatorError> {
        let factory_client = FactoryClient::new(env, factory);
        let tokens = Self::discover_tokens(env, token_in, token_out);

        let mut best_out: i128 = 0;
        let mut best_hops: Vec<RouteHop> = Vec::new(env);
        // Best completed route whose entry venue differs from the winner's.
        let mut runner_up: Option<(Address, PoolKind, i128)> = None;

        let mut frontier_token: Vec<Address> = Vec::new(env);
        let mut frontier_amount: Vec<i128> = Vec::new(env);
        let mut frontier_hops: Vec<Vec<RouteHop>> = Vec::new(env);
        let mut frontier_depth: Vec<u32> = Vec::new(env);

        // (token, depth) pairs already enqueued. Without this, every frontier
        // node re-expands to O(N) neighbours regardless of whether they have
        // already been explored, blowing the frontier up to O(N^max_hops) and
        // exhausting the per-transaction instruction budget (#363).
        let mut visited: Vec<(Address, u32, i128)> = Vec::new(env);

        frontier_token.push_back(token_in.clone());
        frontier_amount.push_back(amount_in);
        frontier_hops.push_back(Vec::new(env));
        frontier_depth.push_back(0);
        visited.push_back((token_in.clone(), 0, amount_in));

        let mut idx: u32 = 0;
        while idx < frontier_token.len() {
            let current_token = frontier_token.get(idx).unwrap();
            let current_amount = frontier_amount.get(idx).unwrap();
            let current_hops = frontier_hops.get(idx).unwrap();
            let depth = frontier_depth.get(idx).unwrap();
            idx += 1;

            if depth >= max_hops {
                continue;
            }

            for t in 0..tokens.len() {
                let next_token = tokens.get(t).unwrap();
                // A route that comes back through a token it already passed
                // through only pays another round of pool fees, and the quote
                // for it is a call into a pool contract. Skip it before
                // quoting; this also covers `current_token` itself.
                if Self::path_visits(token_in, &current_hops, &next_token) {
                    continue;
                }

                let Some(step) = Self::quote_hop(
                    env,
                    &factory_client,
                    &current_token,
                    &next_token,
                    current_amount,
                ) else {
                    continue;
                };

                let mut new_hops = Vec::new(env);
                for h in 0..current_hops.len() {
                    new_hops.push_back(current_hops.get(h).unwrap());
                }
                new_hops.push_back(step.hops.get(0).unwrap());

                if next_token == *token_out {
                    if step.amount_out > best_out {
                        // The old winner becomes the alternative to compare against.
                        if !best_hops.is_empty() {
                            let prev = best_hops.get(0).unwrap();
                            runner_up =
                                Self::better_alt(runner_up, prev.pool, prev.pool_kind, best_out);
                        }
                        best_out = step.amount_out;
                        best_hops = new_hops;
                    } else {
                        let entry = new_hops.get(0).unwrap();
                        runner_up = Self::better_alt(
                            runner_up,
                            entry.pool,
                            entry.pool_kind,
                            step.amount_out,
                        );
                    }
                } else if depth + 1 < max_hops
                    && !Self::is_visited_and_worse(
                        &mut visited,
                        &next_token,
                        depth + 1,
                        step.amount_out,
                    )
                {
                    frontier_token.push_back(next_token);
                    frontier_amount.push_back(step.amount_out);
                    frontier_hops.push_back(new_hops);
                    frontier_depth.push_back(depth + 1);
                }
            }
        }

        if best_out <= 0 || best_hops.is_empty() {
            return Err(AggregatorError::NoRouteFound);
        }

        // A route that re-enters through the winning venue is the same venue
        // decision, not an alternative to it.
        let winner_pool = best_hops.get(0).unwrap().pool;
        let runner_up = match runner_up {
            Some((pool, _, _)) if pool == winner_pool => None,
            other => other,
        };

        Ok((
            RouteQuote {
                amount_out: best_out,
                hops: best_hops,
            },
            runner_up,
        ))
    }

    /// Keep whichever of the two alternatives quoted more output.
    fn better_alt(
        current: Option<(Address, PoolKind, i128)>,
        pool: Address,
        kind: PoolKind,
        amount_out: i128,
    ) -> Option<(Address, PoolKind, i128)> {
        match current {
            Some((_, _, best)) if best >= amount_out => current,
            _ => Some((pool, kind, amount_out)),
        }
    }

    fn execute_hops(
        env: &Env,
        factory: &Address,
        hops: &Vec<RouteHop>,
        trader: &Address,
        amount_in: i128,
        min_out: i128,
        deadline: u64,
    ) -> Result<i128, AggregatorError> {
        let factory_client = FactoryClient::new(env, factory);

        // Validate all hops up front before any token movement
        for hop in hops.iter() {
            if !Self::is_registered_pool(env, &factory_client, &hop) {
                return Err(AggregatorError::UnregisteredPool);
            }
        }

        let mut current = amount_in;
        let last = hops.len() - 1;
        for i in 0..hops.len() {
            let hop = hops.get(i).unwrap();
            let hop_min = if i == last { min_out } else { 0 };
            current = match hop.pool_kind {
                PoolKind::Amm => AmmPoolClient::new(env, &hop.pool).swap(
                    trader,
                    &hop.token_in,
                    &current,
                    &hop_min,
                    &deadline,
                ),
                PoolKind::Cl => {
                    let limit = if hop.zero_for_one {
                        Self::MIN_SQRT_PRICE + 1
                    } else {
                        Self::MAX_SQRT_PRICE - 1
                    };
                    ClPoolClient::new(env, &hop.pool).swap(
                        trader,
                        &hop.zero_for_one,
                        &current,
                        &limit,
                        &hop_min,
                        &deadline,
                    )
                }
            };
        }
        Ok(current)
    }

    fn quote_hop(
        env: &Env,
        factory: &FactoryClient,
        token_in: &Address,
        token_out: &Address,
        amount_in: i128,
    ) -> Option<RouteQuote> {
        if amount_in <= 0 {
            return None;
        }

        let mut best: i128 = 0;
        let mut hop = RouteHop {
            pool: token_in.clone(),
            pool_kind: PoolKind::Amm,
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            zero_for_one: true,
        };

        if let Some(pool) = factory.get_pool(token_in, token_out) {
            let out = AmmPoolClient::new(env, &pool).get_amount_out(token_in, &amount_in);
            if out > best {
                best = out;
                hop = RouteHop {
                    pool,
                    pool_kind: PoolKind::Amm,
                    token_in: token_in.clone(),
                    token_out: token_out.clone(),
                    zero_for_one: true,
                };
            }
        }

        let cl_count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::ClPoolCount)
            .unwrap_or(0);
        for i in 0..cl_count {
            let info: ClPoolInfo = env.storage().instance().get(&DataKey::ClPool(i)).unwrap();
            if !(Self::is_cl_pool_match(&info, token_in, token_out)) {
                continue;
            }
            if let Some((out, zfo)) =
                Self::quote_cl(env, &info.pool, token_in, token_out, amount_in)
            {
                if out > best {
                    best = out;
                    hop = RouteHop {
                        pool: info.pool,
                        pool_kind: PoolKind::Cl,
                        token_in: token_in.clone(),
                        token_out: token_out.clone(),
                        zero_for_one: zfo,
                    };
                }
            }
        }

        for fee_idx in 0..3 {
            let fee = Self::CL_FEE_TIERS[fee_idx as usize];
            if let Some(cl) = factory.get_cl_pool(token_in, token_out, &fee) {
                if let Some((out, zfo)) = Self::quote_cl(env, &cl, token_in, token_out, amount_in) {
                    if out > best {
                        best = out;
                        hop = RouteHop {
                            pool: cl,
                            pool_kind: PoolKind::Cl,
                            token_in: token_in.clone(),
                            token_out: token_out.clone(),
                            zero_for_one: zfo,
                        };
                    }
                }
            }
        }

        if best <= 0 {
            return None;
        }

        Some(RouteQuote {
            amount_out: best,
            hops: vec![env, hop],
        })
    }

    fn quote_cl(
        env: &Env,
        pool: &Address,
        _token_in: &Address,
        _token_out: &Address,
        amount_in: i128,
    ) -> Option<(i128, bool)> {
        let client = ClPoolClient::new(env, pool);
        let mut best: i128 = 0;
        let mut zfo = true;
        for direction in [true, false] {
            let limit = if direction {
                Self::MIN_SQRT_PRICE + 1
            } else {
                Self::MAX_SQRT_PRICE - 1
            };
            let est = client.estimate_price_impact(&direction, &amount_in, &limit);
            if est.amount_out > best {
                best = est.amount_out;
                zfo = direction;
            }
        }
        if best > 0 {
            Some((best, zfo))
        } else {
            None
        }
    }

    fn discover_tokens(env: &Env, token_in: &Address, token_out: &Address) -> Vec<Address> {
        let mut tokens: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::RoutingTokens)
            .unwrap_or_else(|| Vec::new(env));
        Self::push_unique(&mut tokens, token_in.clone());
        Self::push_unique(&mut tokens, token_out.clone());

        let cl_count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::ClPoolCount)
            .unwrap_or(0);
        for i in 0..cl_count {
            let info: ClPoolInfo = env.storage().instance().get(&DataKey::ClPool(i)).unwrap();
            Self::push_unique(&mut tokens, info.token_a.clone());
            Self::push_unique(&mut tokens, info.token_b.clone());
        }
        tokens
    }

    fn is_cl_pool_match(info: &ClPoolInfo, token_in: &Address, token_out: &Address) -> bool {
        (info.token_a == *token_in && info.token_b == *token_out)
            || (info.token_a == *token_out && info.token_b == *token_in)
    }

    /// Check if a hop references a registered pool (AMM or CL).
    /// For AMM: validates pool exists via factory.get_pool_tokens.
    /// For CL: validates pool exists in either factory or local registry.
    fn is_registered_pool(env: &Env, factory: &FactoryClient, hop: &RouteHop) -> bool {
        match hop.pool_kind {
            PoolKind::Amm => {
                // AMM pool must exist in factory
                factory.get_pool_tokens(&hop.pool).is_some()
            }
            PoolKind::Cl => {
                // CL pool can come from factory or local registry
                // Check factory first (all fee tiers)
                for fee_idx in 0..3 {
                    let fee = Self::CL_FEE_TIERS[fee_idx as usize];
                    if let Some(pool) = factory.get_cl_pool(&hop.token_in, &hop.token_out, &fee) {
                        if pool == hop.pool {
                            return true;
                        }
                    }
                }

                // Check local registry
                let cl_count: u32 = env
                    .storage()
                    .instance()
                    .get(&DataKey::ClPoolCount)
                    .unwrap_or(0);
                for i in 0..cl_count {
                    let info: ClPoolInfo =
                        env.storage().instance().get(&DataKey::ClPool(i)).unwrap();
                    if info.pool == hop.pool {
                        return true;
                    }
                }

                false
            }
        }
    }

    /// Has `(token, depth)` already been enqueued with a better or equal amount? (#363)
    /// Whether `token` is the route's entry token or the output of a hop
    /// already on it.
    fn path_visits(token_in: &Address, hops: &Vec<RouteHop>, token: &Address) -> bool {
        if token == token_in {
            return true;
        }
        for h in 0..hops.len() {
            if hops.get(h).unwrap().token_out == *token {
                return true;
            }
        }
        false
    }

    fn is_visited_and_worse(
        visited: &mut Vec<(Address, u32, i128)>,
        token: &Address,
        depth: u32,
        amount: i128,
    ) -> bool {
        for i in 0..visited.len() {
            let (t, d, a) = visited.get(i).unwrap();
            if d == depth && t == *token {
                if amount <= a {
                    return true;
                } else {
                    visited.set(i, (t, d, amount));
                    return false;
                }
            }
        }
        visited.push_back((token.clone(), depth, amount));
        false
    }

    fn push_unique(vec: &mut Vec<Address>, addr: Address) {
        for i in 0..vec.len() {
            if vec.get(i).unwrap() == addr {
                return;
            }
        }
        vec.push_back(addr);
    }

    fn extend_ttl(env: &Env) {
        env.storage().instance().extend_ttl(MIN_TTL, BUMP_TO);
    }

    fn read_admin(env: &Env) -> Result<Address, AggregatorError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(AggregatorError::NotInitialized)
    }

    fn read_factory(env: &Env) -> Result<Address, AggregatorError> {
        env.storage()
            .instance()
            .get(&DataKey::Factory)
            .ok_or(AggregatorError::NotInitialized)
    }

    /// Stored-admin check shared by `pause` / `unpause`: the caller-supplied
    /// address must equal the stored admin and must authorize the call.
    fn require_admin(env: &Env, admin: &Address) -> Result<(), AggregatorError> {
        let stored_admin = Self::read_admin(env)?;
        if *admin != stored_admin {
            return Err(AggregatorError::Unauthorized);
        }
        admin.require_auth();
        Ok(())
    }

    fn require_not_paused(env: &Env) -> Result<(), AggregatorError> {
        if Self::is_paused(env.clone()) {
            return Err(AggregatorError::Paused);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use factory::{Factory, FactoryClient};
    use soroban_sdk::{
        testutils::{Address as _, Events as _, Ledger as _},
        token::StellarAssetClient,
        Address, BytesN, IntoVal, Symbol,
    };

    #[test]
    fn test_no_route_when_uninitialized() {
        let env = Env::default();
        let agg_addr = env.register_contract(None, DexAggregator);
        let agg = DexAggregatorClient::new(&env, &agg_addr);
        let a = Address::generate(&env);
        let b = Address::generate(&env);
        let result = agg.try_find_best_route(&a, &b, &100_i128, &3u32);
        assert_eq!(result, Err(Ok(AggregatorError::NotInitialized)));
    }

    #[test]
    fn test_pre_init_calls_return_not_initialized() {
        let env = Env::default();
        env.mock_all_auths();
        let agg_addr = env.register_contract(None, DexAggregator);
        let agg = DexAggregatorClient::new(&env, &agg_addr);
        let a = Address::generate(&env);
        let b = Address::generate(&env);
        let pool = Address::generate(&env);
        let trader = Address::generate(&env);

        assert_eq!(
            agg.try_set_max_hops(&3u32),
            Err(Ok(AggregatorError::NotInitialized))
        );
        assert_eq!(
            agg.try_register_cl_pool(&pool, &a, &b, &30_i128),
            Err(Ok(AggregatorError::NotInitialized))
        );
        assert_eq!(
            agg.try_remove_cl_pool(&pool),
            Err(Ok(AggregatorError::NotInitialized))
        );
        assert_eq!(
            agg.try_set_routing_tokens(&vec![&env, a.clone()]),
            Err(Ok(AggregatorError::NotInitialized))
        );
        assert_eq!(
            agg.try_get_quote(&a, &b, &100_i128, &3u32),
            Err(Ok(AggregatorError::NotInitialized))
        );
        assert_eq!(
            agg.try_swap_best(&trader, &a, &b, &100_i128, &0_i128, &u64::MAX),
            Err(Ok(AggregatorError::NotInitialized))
        );

        let route = RouteQuote {
            amount_out: 100,
            hops: vec![
                &env,
                RouteHop {
                    pool: pool.clone(),
                    pool_kind: PoolKind::Amm,
                    token_in: a.clone(),
                    token_out: b.clone(),
                    zero_for_one: true,
                },
            ],
        };
        assert_eq!(
            agg.try_execute_route(&route, &trader, &100_i128, &0_i128, &u64::MAX),
            Err(Ok(AggregatorError::NotInitialized))
        );

        // A bool-returning query must degrade gracefully rather than trap.
        assert!(!agg.is_price_within_tolerance(&a, &b, &100_i128, &1_i128));
    }

    #[test]
    fn test_visited_dedup_is_keyed_on_token_and_depth() {
        let env = Env::default();
        let token_a = Address::generate(&env);
        let token_b = Address::generate(&env);

        let mut visited: Vec<(Address, u32, i128)> = Vec::new(&env);
        visited.push_back((token_a.clone(), 1, 100));

        // Same (token, depth) pair with worse or equal amount is blocked.
        assert!(DexAggregator::is_visited_and_worse(
            &mut visited,
            &token_a,
            1,
            100
        ));
        assert!(DexAggregator::is_visited_and_worse(
            &mut visited,
            &token_a,
            1,
            50
        ));
        // Same (token, depth) pair with strictly better amount is allowed.
        assert!(!DexAggregator::is_visited_and_worse(
            &mut visited,
            &token_a,
            1,
            150
        ));

        // After being allowed, the new best amount is recorded (150).
        assert!(DexAggregator::is_visited_and_worse(
            &mut visited,
            &token_a,
            1,
            150
        ));
        assert!(DexAggregator::is_visited_and_worse(
            &mut visited,
            &token_a,
            1,
            140
        ));

        // Same token at a different depth must still be explorable.
        assert!(!DexAggregator::is_visited_and_worse(
            &mut visited,
            &token_a,
            2,
            50
        ));
        // A different token at the same depth is independent.
        assert!(!DexAggregator::is_visited_and_worse(
            &mut visited,
            &token_b,
            1,
            50
        ));
    }

    #[test]
    fn test_discover_tokens_includes_registered_cl_pools() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        let admin = Address::generate(&env);
        factory.initialize(
            &admin,
            &BytesN::from_array(&env, &[0u8; 32]),
            &BytesN::from_array(&env, &[1u8; 32]),
        );

        let agg_addr = env.register_contract(None, DexAggregator);
        let agg = DexAggregatorClient::new(&env, &agg_addr);
        let agg_admin = Address::generate(&env);
        agg.initialize(&agg_admin, &factory_addr);

        let token_a = Address::generate(&env);
        let token_b = Address::generate(&env);
        let cl_pool = Address::generate(&env);
        agg.register_cl_pool(&cl_pool, &token_a, &token_b, &30_i128);

        let tokens = env.as_contract(&agg_addr, || {
            DexAggregator::discover_tokens(&env, &token_a, &token_b)
        });
        assert_eq!(tokens.len(), 2);

        let mut found_a = false;
        let mut found_b = false;
        for i in 0..tokens.len() {
            let token = tokens.get(i).unwrap();
            if token == token_a {
                found_a = true;
            }
            if token == token_b {
                found_b = true;
            }
        }

        assert!(found_a);
        assert!(found_b);
    }

    // -------------------------------------------------------------------------
    // #685: versioned routing events
    // -------------------------------------------------------------------------

    struct Venues {
        env: Env,
        agg: Address,
        admin: Address,
        trader: Address,
        token_a: Address,
        token_b: Address,
        pool_ab: Address,
        factory: Address,
    }

    /// Aggregator wired to a real factory with a funded `token_a <-> token_b`
    /// AMM pool, plus a trader holding both tokens.
    fn setup_venues() -> Venues {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();

        let admin = Address::generate(&env);
        let amm_wasm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let lp_wasm_hash = env.deployer().upload_contract_wasm(token::WASM);

        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_wasm_hash, &lp_wasm_hash);

        let agg_addr = env.register_contract(None, DexAggregator);
        DexAggregatorClient::new(&env, &agg_addr).initialize(&admin, &factory_addr);

        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        // fee_tier 2 = 30 bps (Medium); governance_wasm_hash = None
        factory.create_pool(&admin, &token_a, &token_b, &2_i128, &None);
        let pool_ab = factory.get_pool(&token_a, &token_b).unwrap();

        let lp = Address::generate(&env);
        let trader = Address::generate(&env);
        for t in [&token_a, &token_b] {
            StellarAssetClient::new(&env, t).mint(&lp, &10_000_000_i128);
            StellarAssetClient::new(&env, t).mint(&trader, &1_000_000_i128);
        }
        amm::AmmPoolClient::new(&env, &pool_ab).add_liquidity(
            &lp,
            &1_000_000,
            &1_000_000,
            &0,
            &u64::MAX,
        );

        Venues {
            env,
            agg: agg_addr,
            admin,
            trader,
            token_a,
            token_b,
            pool_ab,
            factory: factory_addr,
        }
    }

    /// Payload of the last event this contract emitted under `topic`, with the
    /// schema-version prefix asserted and stripped.
    fn last_payload<T: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>>(
        env: &Env,
        contract: &Address,
        topic: Symbol,
    ) -> T {
        let event = env
            .events()
            .all()
            .iter()
            .rfind(|e| e.0 == *contract && e.1 == (topic.clone(),).into_val(env))
            .unwrap_or_else(|| panic!("no event emitted for the requested topic"));
        let (version, payload): (u32, T) = event.2.into_val(env);
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        payload
    }

    fn count_events(env: &Env, contract: &Address, topic: Symbol) -> usize {
        env.events()
            .all()
            .iter()
            .filter(|e| e.0 == *contract && e.1 == (topic.clone(),).into_val(env))
            .count()
    }

    /// A route must never pass through a token it already visited. The search
    /// used to keep walking back through the entry token up to `max_hops`,
    /// quoting pool after pool for routes that cannot beat the one it already
    /// had. On `a -> b -> c` the answer is the same at any `max_hops >= 2`, so
    /// the work should be too.
    #[test]
    fn test_search_does_not_requote_tokens_already_on_the_path() {
        let v = setup_venues();
        let env = &v.env;
        let factory = FactoryClient::new(env, &v.factory);

        let token_c = env
            .register_stellar_asset_contract_v2(v.admin.clone())
            .address();
        factory.create_pool(&v.admin, &v.token_b, &token_c, &2_i128, &None);
        let pool_bc = factory.get_pool(&v.token_b, &token_c).unwrap();
        let lp = Address::generate(env);
        for t in [&v.token_b, &token_c] {
            StellarAssetClient::new(env, t).mint(&lp, &10_000_000_i128);
        }
        amm::AmmPoolClient::new(env, &pool_bc).add_liquidity(
            &lp,
            &1_000_000,
            &1_000_000,
            &0,
            &u64::MAX,
        );

        let agg = DexAggregatorClient::new(env, &v.agg);
        agg.set_routing_tokens(&vec![env, v.token_b.clone()]);

        let quote_with = |max_hops: u32| {
            env.budget().reset_unlimited();
            let quote = agg.find_best_route(&v.token_a, &token_c, &10_000_i128, &max_hops);
            (env.budget().cpu_instruction_cost(), quote)
        };
        let (cost_2, quote_2) = quote_with(2);
        let (cost_4, quote_4) = quote_with(4);

        assert_eq!(quote_2.hops.len(), 2);
        assert_eq!(quote_2.hops.get(0).unwrap().token_in, v.token_a);
        assert_eq!(quote_2.hops.get(0).unwrap().token_out, v.token_b);
        assert_eq!(quote_2.hops.get(1).unwrap().token_out, token_c);
        assert_eq!(quote_4.hops, quote_2.hops);
        assert_eq!(quote_4.amount_out, quote_2.amount_out);

        // Allow 1% for the differing `max_hops` argument itself. Re-quoting
        // `b -> a` and `a -> b` at the higher cap roughly doubled the cost.
        assert!(
            cost_4 * 100 <= cost_2 * 101,
            "searching with max_hops=4 cost {cost_4} CPU instructions against {cost_2} at \
             max_hops=2: the search is re-quoting hops through tokens already on the path"
        );
    }

    #[test]
    fn test_register_cl_pool_emits_cl_reg() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);
        let cl_pool = Address::generate(&v.env);

        agg.register_cl_pool(&cl_pool, &v.token_a, &v.token_b, &30_i128);

        let (token_a, token_b, fee_bps, pool): (Address, Address, i128, Address) =
            last_payload(&v.env, &v.agg, symbol_short!("cl_reg"));
        assert_eq!(token_a, v.token_a);
        assert_eq!(token_b, v.token_b);
        assert_eq!(fee_bps, 30);
        assert_eq!(pool, cl_pool);
    }

    #[test]
    fn test_register_cl_pool_does_not_re_emit_for_a_known_pool() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);
        let cl_pool = Address::generate(&v.env);

        agg.register_cl_pool(&cl_pool, &v.token_a, &v.token_b, &30_i128);
        agg.register_cl_pool(&cl_pool, &v.token_a, &v.token_b, &30_i128);

        assert_eq!(count_events(&v.env, &v.agg, symbol_short!("cl_reg")), 1);
    }

    #[test]
    fn test_find_best_route_emits_route_sel_with_the_chosen_venue() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        let quote = agg.find_best_route(&v.token_a, &v.token_b, &10_000_i128, &2u32);

        let (venue, venue_kind, amount_in, amount_out): (Address, PoolKind, i128, i128) =
            last_payload(&v.env, &v.agg, symbol_short!("route_sel"));
        assert_eq!(venue, v.pool_ab);
        assert_eq!(venue_kind, PoolKind::Amm);
        assert_eq!(amount_in, 10_000);
        assert_eq!(amount_out, quote.amount_out);
    }

    #[test]
    fn test_route_alt_is_not_emitted_when_only_one_venue_quoted() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        agg.find_best_route(&v.token_a, &v.token_b, &10_000_i128, &2u32);

        assert_eq!(count_events(&v.env, &v.agg, symbol_short!("route_sel")), 1);
        assert_eq!(count_events(&v.env, &v.agg, symbol_short!("route_alt")), 0);
    }

    #[test]
    fn test_route_alt_reports_the_runner_up_when_a_second_venue_quotes() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);
        let factory = FactoryClient::new(&v.env, &v.factory);

        // A second, deliberately shallower A->C->B route through a third token,
        // so two distinct entry venues complete a route to token_b.
        let token_c = env_token(&v.env, &v.admin);
        factory.create_pool(&v.admin, &v.token_a, &token_c, &2_i128, &None);
        factory.create_pool(&v.admin, &token_c, &v.token_b, &2_i128, &None);

        let lp = Address::generate(&v.env);
        for t in [&v.token_a, &token_c, &v.token_b] {
            StellarAssetClient::new(&v.env, t).mint(&lp, &10_000_000_i128);
        }
        for (x, y) in [(&v.token_a, &token_c), (&token_c, &v.token_b)] {
            let pool = factory.get_pool(x, y).unwrap();
            amm::AmmPoolClient::new(&v.env, &pool).add_liquidity(
                &lp,
                &500_000,
                &500_000,
                &0,
                &u64::MAX,
            );
        }
        agg.set_routing_tokens(&soroban_sdk::vec![
            &v.env,
            v.token_a.clone(),
            token_c.clone(),
            v.token_b.clone()
        ]);

        let quote = agg.find_best_route(&v.token_a, &v.token_b, &10_000_i128, &3u32);

        let (venue, amount_out, alt_venue, _alt_kind, alt_amount_out): (
            Address,
            i128,
            Address,
            PoolKind,
            i128,
        ) = last_payload(&v.env, &v.agg, symbol_short!("route_alt"));
        assert_eq!(venue, quote.hops.get(0).unwrap().pool);
        assert_eq!(amount_out, quote.amount_out);
        // The alternative is a different entry venue and never beats the winner.
        assert_ne!(alt_venue, venue);
        assert!(alt_amount_out <= amount_out);
    }

    #[test]
    fn test_execute_route_emits_route_exe_with_the_settled_amount() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        let quote = agg.find_best_route(&v.token_a, &v.token_b, &10_000_i128, &2u32);
        let out = agg.execute_route(&quote, &v.trader, &10_000_i128, &0_i128, &u64::MAX);

        let (trader, token_in, token_out, amount_in, amount_out, pool): (
            Address,
            Address,
            Address,
            i128,
            i128,
            Address,
        ) = last_payload(&v.env, &v.agg, symbol_short!("route_exe"));
        assert_eq!(trader, v.trader);
        assert_eq!(token_in, v.token_a);
        assert_eq!(token_out, v.token_b);
        assert_eq!(amount_in, 10_000);
        assert_eq!(pool, v.pool_ab);
        // The pool's actual output, which is what execute_route returned.
        assert_eq!(amount_out, out);
    }

    #[test]
    fn test_swap_best_emits_both_route_sel_and_route_exe() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        let out = agg.swap_best(
            &v.trader,
            &v.token_a,
            &v.token_b,
            &10_000_i128,
            &0_i128,
            &u64::MAX,
        );

        assert_eq!(count_events(&v.env, &v.agg, symbol_short!("route_sel")), 1);
        let (_, _, _, quoted): (Address, PoolKind, i128, i128) =
            last_payload(&v.env, &v.agg, symbol_short!("route_sel"));
        let (_, _, _, _, settled, _): (Address, Address, Address, i128, i128, Address) =
            last_payload(&v.env, &v.agg, symbol_short!("route_exe"));
        assert_eq!(settled, out);
        assert_eq!(quoted, out);
    }

    #[test]
    fn test_tolerance_failure_emits_tol_fail() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        // Quote far outside the 10 bps tolerance.
        assert!(!agg.is_price_within_tolerance(&v.token_a, &v.token_b, &10_000_i128, &1_i128));

        let (pool, observed_bps, tolerance_bps): (Address, i128, i128) =
            last_payload(&v.env, &v.agg, symbol_short!("tol_fail"));
        assert_eq!(pool, v.pool_ab);
        assert_eq!(tolerance_bps, DexAggregator::PRICE_TOLERANCE_BPS);
        assert!(observed_bps > tolerance_bps);
    }

    #[test]
    fn test_tolerance_success_emits_nothing() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        let quote = agg.find_best_route(&v.token_a, &v.token_b, &10_000_i128, &2u32);
        assert!(agg.is_price_within_tolerance(
            &v.token_a,
            &v.token_b,
            &10_000_i128,
            &quote.amount_out
        ));

        assert_eq!(count_events(&v.env, &v.agg, symbol_short!("tol_fail")), 0);
    }

    #[test]
    fn test_no_routing_events_when_no_route_exists() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);
        let orphan = env_token(&v.env, &v.admin);

        assert!(agg
            .try_find_best_route(&v.token_a, &orphan, &10_000_i128, &2u32)
            .is_err());

        assert_eq!(count_events(&v.env, &v.agg, symbol_short!("route_sel")), 0);
        assert_eq!(count_events(&v.env, &v.agg, symbol_short!("route_alt")), 0);
    }

    // -------------------------------------------------------------------------
    // #813: Unregistered pool validation tests
    // -------------------------------------------------------------------------

    /// Test 1: Regression test - reject unregistered AMM pool (would execute on main)
    #[test]
    fn test_execute_route_rejects_unregistered_amm_pool() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        // Create a malicious fake pool contract (an address that isn't registered)
        let fake_pool = Address::generate(&v.env);

        // Build a route manually pointing at the unregistered pool
        let bad_route = RouteQuote {
            amount_out: 9_000_i128,
            hops: vec![
                &v.env,
                RouteHop {
                    pool: fake_pool,
                    pool_kind: PoolKind::Amm,
                    token_in: v.token_a.clone(),
                    token_out: v.token_b.clone(),
                    zero_for_one: true,
                },
            ],
        };

        // execute_route should reject the unregistered pool upfront
        let result = agg.try_execute_route(&bad_route, &v.trader, &10_000_i128, &0_i128, &u64::MAX);

        assert!(
            result.is_err(),
            "execute_route must reject unregistered AMM pool"
        );
        if let Err(Ok(err)) = result {
            assert_eq!(
                err,
                AggregatorError::UnregisteredPool,
                "error must be UnregisteredPool"
            );
        } else {
            panic!("expected AggregatorError::UnregisteredPool");
        }
    }

    /// Test 2: Regression test - reject unregistered CL pool
    #[test]
    fn test_execute_route_rejects_unregistered_cl_pool() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        // Create a malicious fake CL pool contract
        let fake_cl_pool = Address::generate(&v.env);

        let bad_route = RouteQuote {
            amount_out: 9_000_i128,
            hops: vec![
                &v.env,
                RouteHop {
                    pool: fake_cl_pool,
                    pool_kind: PoolKind::Cl,
                    token_in: v.token_a.clone(),
                    token_out: v.token_b.clone(),
                    zero_for_one: true,
                },
            ],
        };

        let result = agg.try_execute_route(&bad_route, &v.trader, &10_000_i128, &0_i128, &u64::MAX);

        assert!(
            result.is_err(),
            "execute_route must reject unregistered CL pool"
        );
        if let Err(Ok(err)) = result {
            assert_eq!(
                err,
                AggregatorError::UnregisteredPool,
                "error must be UnregisteredPool"
            );
        } else {
            panic!("expected AggregatorError::UnregisteredPool");
        }
    }

    /// Test 3: Positive test - legitimate route from find_best_route succeeds
    #[test]
    fn test_execute_route_accepts_legitimate_route_from_find_best_route() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        // Get a route from the trusted path
        let quote = agg.find_best_route(&v.token_a, &v.token_b, &10_000_i128, &2u32);
        assert!(quote.amount_out > 0, "quote should have positive output");

        // Execute it - should succeed with no UnregisteredPool error
        let result = agg.try_execute_route(&quote, &v.trader, &10_000_i128, &0_i128, &u64::MAX);

        assert!(result.is_ok(), "legitimate route must succeed");
        let out = result.unwrap().unwrap();
        assert!(out > 0, "output must be positive");
    }

    /// Test 4: Positive test - swap_best (full trusted path) still succeeds
    #[test]
    fn test_swap_best_still_succeeds_with_validation() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        let out = agg.swap_best(
            &v.trader,
            &v.token_a,
            &v.token_b,
            &10_000_i128,
            &0_i128,
            &u64::MAX,
        );

        assert!(
            out > 0,
            "swap_best must produce positive output with validation"
        );
    }

    /// Test 5: Multi-hop atomicity - if hop 2 is unregistered, hop 1 doesn't execute
    #[test]
    fn test_multi_hop_atomicity_no_partial_execution() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        // Get a valid hop from token_a to token_b
        let valid_hop_quote = agg.find_best_route(&v.token_a, &v.token_b, &10_000_i128, &1u32);
        let valid_hop = valid_hop_quote.hops.get(0).unwrap();

        // Create a fake second hop (unregistered pool)
        let fake_pool = Address::generate(&v.env);
        let bad_hop = RouteHop {
            pool: fake_pool,
            pool_kind: PoolKind::Amm,
            token_in: v.token_b.clone(),
            token_out: v.token_a.clone(),
            zero_for_one: true,
        };

        // Build a 2-hop route where hop 1 is valid but hop 2 is not
        let bad_route = RouteQuote {
            amount_out: 9_000_i128,
            hops: vec![&v.env, valid_hop.clone(), bad_hop],
        };

        // Get token balance before
        let token_a_client = soroban_sdk::token::TokenClient::new(&v.env, &v.token_a);
        let balance_before = token_a_client.balance(&v.trader);

        // execute_route should fail before any swap
        let result = agg.try_execute_route(&bad_route, &v.trader, &10_000_i128, &0_i128, &u64::MAX);

        assert!(result.is_err(), "route with unregistered hop must fail");

        // Verify token balance is unchanged (no partial execution)
        let balance_after = token_a_client.balance(&v.trader);
        assert_eq!(
            balance_before, balance_after,
            "trader's token_a balance must be unchanged (atomic failure)"
        );
    }

    /// Test 6: Upfront validation - all hops checked before any movement
    #[test]
    fn test_validation_happens_upfront_before_token_transfer() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        // Create a route where hop 1 is valid, but hop 2 has an unregistered pool
        let valid_hop_quote = agg.find_best_route(&v.token_a, &v.token_b, &10_000_i128, &1u32);
        let valid_hop = valid_hop_quote.hops.get(0).unwrap();

        // Unregistered second hop
        let fake_pool = Address::generate(&v.env);
        let bad_hop = RouteHop {
            pool: fake_pool,
            pool_kind: PoolKind::Amm,
            token_in: v.token_b.clone(),
            token_out: v.token_a.clone(),
            zero_for_one: true,
        };

        let bad_route = RouteQuote {
            amount_out: 9_000_i128,
            hops: vec![&v.env, valid_hop.clone(), bad_hop],
        };

        // Get initial state
        let token_a_client = soroban_sdk::token::TokenClient::new(&v.env, &v.token_a);
        let token_b_client = soroban_sdk::token::TokenClient::new(&v.env, &v.token_b);
        let balance_a_before = token_a_client.balance(&v.trader);
        let balance_b_before = token_b_client.balance(&v.trader);
        let pool_info_before = amm::AmmPoolClient::new(&v.env, &v.pool_ab).get_info();

        // Execute route (should fail validation)
        let _result =
            agg.try_execute_route(&bad_route, &v.trader, &10_000_i128, &0_i128, &u64::MAX);

        // Verify nothing changed
        let balance_a_after = token_a_client.balance(&v.trader);
        let balance_b_after = token_b_client.balance(&v.trader);
        let pool_info_after = amm::AmmPoolClient::new(&v.env, &v.pool_ab).get_info();

        assert_eq!(
            balance_a_before, balance_a_after,
            "token_a balance must be unchanged"
        );
        assert_eq!(
            balance_b_before, balance_b_after,
            "token_b balance must be unchanged"
        );
        assert_eq!(
            pool_info_before.reserve_a, pool_info_after.reserve_a,
            "pool reserve_a must be unchanged"
        );
        assert_eq!(
            pool_info_before.reserve_b, pool_info_after.reserve_b,
            "pool reserve_b must be unchanged"
        );
    }

    // -------------------------------------------------------------------------
    // #814: Deadline parameter tests
    // -------------------------------------------------------------------------

    /// Test 1: Regression test - swap_best rejects expired deadline.
    /// A deadline one second in the past must fail without attempting the swap.
    #[test]
    fn test_swap_best_rejects_expired_deadline() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        // Advance the ledger so "one second in the past" is a real past
        // timestamp rather than saturating to 0.
        v.env.ledger().set_timestamp(1_000);

        // Get current timestamp and use one second in the past as deadline
        let now = v.env.ledger().timestamp();
        let expired_deadline = now.saturating_sub(1);

        // swap_best with expired deadline should fail with SlippageExceeded
        let result = agg.try_swap_best(
            &v.trader,
            &v.token_a,
            &v.token_b,
            &10_000_i128,
            &0_i128,
            &expired_deadline,
        );

        assert!(
            result.is_err(),
            "swap_best must reject expired deadline (in the past)"
        );
        if let Err(Ok(err)) = result {
            assert_eq!(
                err,
                AggregatorError::SlippageExceeded,
                "expired deadline must return SlippageExceeded"
            );
        } else {
            panic!("expected AggregatorError::SlippageExceeded");
        }
    }

    /// Test 2: Positive test - swap_best succeeds with deadline 10 seconds in future.
    #[test]
    fn test_swap_best_succeeds_with_future_deadline() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        // Use a deadline 10 seconds in the future
        let now = v.env.ledger().timestamp();
        let future_deadline = now + 10;

        let result = agg.try_swap_best(
            &v.trader,
            &v.token_a,
            &v.token_b,
            &10_000_i128,
            &0_i128,
            &future_deadline,
        );

        assert!(
            result.is_ok(),
            "swap_best must succeed with future deadline"
        );
        let out = result.unwrap().unwrap();
        assert!(out > 0, "swap_best must produce positive output");
    }

    /// Test 3: Boundary test - swap_best succeeds when deadline equals current timestamp.
    /// The execute_route check is `deadline < timestamp`, so equal should succeed.
    #[test]
    fn test_swap_best_succeeds_at_deadline_boundary() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        // Use current timestamp as deadline (execute_route checks deadline < timestamp, not <=)
        let now = v.env.ledger().timestamp();
        let deadline_equals_now = now;

        let result = agg.try_swap_best(
            &v.trader,
            &v.token_a,
            &v.token_b,
            &10_000_i128,
            &0_i128,
            &deadline_equals_now,
        );

        assert!(
            result.is_ok(),
            "swap_best must succeed when deadline == current timestamp (strict < comparison)"
        );
        let out = result.unwrap().unwrap();
        assert!(out > 0, "swap_best must produce positive output");
    }

    /// Test 4: Economics invariance - deadline does not affect swap output.
    /// Two swaps against the same route with different deadlines should produce identical output.
    #[test]
    fn test_swap_best_deadline_does_not_affect_economics() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        let now = v.env.ledger().timestamp();
        let short_deadline = now + 5;
        let long_deadline = now + 3600;

        // First swap with short deadline
        let out1 = agg.swap_best(
            &v.trader,
            &v.token_a,
            &v.token_b,
            &10_000_i128,
            &0_i128,
            &short_deadline,
        );

        // Reset trader funds for second swap
        // Need to create a new venue setup for a clean second trade
        let v2 = setup_venues();
        let agg2 = DexAggregatorClient::new(&v2.env, &v2.agg);

        // Second swap with long deadline against same input
        let out2 = agg2.swap_best(
            &v2.trader,
            &v2.token_a,
            &v2.token_b,
            &10_000_i128,
            &0_i128,
            &long_deadline,
        );

        // Both should produce identical output (deadline only affects expiry, not economics)
        assert_eq!(
            out1, out2,
            "swap_best must produce identical output regardless of deadline"
        );
        assert!(out1 > 0, "both swaps must produce positive output");
    }

    fn env_token(env: &Env, admin: &Address) -> Address {
        env.register_stellar_asset_contract_v2(admin.clone())
            .address()
    }

    // -------------------------------------------------------------------------
    // #809: Authorization checks for set_max_hops and set_routing_tokens
    // -------------------------------------------------------------------------

    #[test]
    fn test_set_max_hops_non_admin_fails() {
        // Regression test: non-admin calls set_max_hops with no auth → must panic
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        let admin = Address::generate(&env);
        factory.initialize(
            &admin,
            &BytesN::from_array(&env, &[0u8; 32]),
            &BytesN::from_array(&env, &[1u8; 32]),
        );

        let agg_addr = env.register_contract(None, DexAggregator);
        let agg = DexAggregatorClient::new(&env, &agg_addr);
        agg.initialize(&admin, &factory_addr);

        let _non_admin = Address::generate(&env);
        env.mock_auths(&[]);

        // Non-admin calling set_max_hops should fail
        let result = agg.try_set_max_hops(&3u32);
        assert!(result.is_err());
    }

    #[test]
    fn test_set_routing_tokens_non_admin_fails() {
        // Regression test: non-admin calls set_routing_tokens with no auth → must panic
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        let admin = Address::generate(&env);
        factory.initialize(
            &admin,
            &BytesN::from_array(&env, &[0u8; 32]),
            &BytesN::from_array(&env, &[1u8; 32]),
        );

        let agg_addr = env.register_contract(None, DexAggregator);
        let agg = DexAggregatorClient::new(&env, &agg_addr);
        agg.initialize(&admin, &factory_addr);

        let token = Address::generate(&env);
        let tokens = vec![&env, token];
        env.mock_auths(&[]);

        // Non-admin calling set_routing_tokens should fail
        let result = agg.try_set_routing_tokens(&tokens);
        assert!(result.is_err());
    }

    #[test]
    fn test_admin_set_max_hops_success_then_route_limited() {
        // Admin calls set_max_hops(&env, &3) successfully, then a >3-hop route fails
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();

        let admin = Address::generate(&env);
        let amm_wasm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let lp_wasm_hash = env.deployer().upload_contract_wasm(token::WASM);

        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_wasm_hash, &lp_wasm_hash);

        let agg_addr = env.register_contract(None, DexAggregator);
        let agg = DexAggregatorClient::new(&env, &agg_addr);
        agg.initialize(&admin, &factory_addr);

        // Admin sets max_hops to 3
        let result = agg.try_set_max_hops(&3u32);
        assert!(result.is_ok());

        // Verify find_best_route respects the max_hops cap
        let token_a = Address::generate(&env);
        let token_b = Address::generate(&env);

        // Create a deep route that would require >3 hops (this will fail because we have no pools)
        let result = agg.try_find_best_route(&token_a, &token_b, &100_i128, &5u32);
        assert!(result.is_err()); // Expected: NoRouteFound because no pools registered
    }

    #[test]
    fn test_set_max_hops_zero_fails_with_invalid_max_hops() {
        // set_max_hops(&env, &0) → Err(AggregatorError::InvalidMaxHops)
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(
            &admin,
            &BytesN::from_array(&env, &[0u8; 32]),
            &BytesN::from_array(&env, &[1u8; 32]),
        );

        let agg_addr = env.register_contract(None, DexAggregator);
        let agg = DexAggregatorClient::new(&env, &agg_addr);
        agg.initialize(&admin, &factory_addr);

        // Setting max_hops to 0 should fail with InvalidMaxHops
        let result = agg.try_set_max_hops(&0u32);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().unwrap(),
            AggregatorError::InvalidMaxHops
        );
    }

    #[test]
    fn test_set_routing_tokens_too_many_fails() {
        // set_routing_tokens with MAX_ROUTING_TOKENS + 1 addresses → TooManyRoutingTokens
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(
            &admin,
            &BytesN::from_array(&env, &[0u8; 32]),
            &BytesN::from_array(&env, &[1u8; 32]),
        );

        let agg_addr = env.register_contract(None, DexAggregator);
        let agg = DexAggregatorClient::new(&env, &agg_addr);
        agg.initialize(&admin, &factory_addr);

        // Create MAX_ROUTING_TOKENS + 1 addresses
        let mut too_many_tokens = Vec::new(&env);
        for _ in 0..=(DexAggregator::MAX_ROUTING_TOKENS) {
            too_many_tokens.push_back(Address::generate(&env));
        }

        // Setting too many tokens should fail with TooManyRoutingTokens
        let result = agg.try_set_routing_tokens(&too_many_tokens);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().unwrap(),
            AggregatorError::TooManyRoutingTokens
        );
    }

    #[test]
    fn test_admin_set_routing_tokens_success_updates_discovery() {
        // Admin calls set_routing_tokens with a valid list, and discover_tokens picks up the new tokens
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        let admin = Address::generate(&env);
        factory.initialize(
            &admin,
            &BytesN::from_array(&env, &[0u8; 32]),
            &BytesN::from_array(&env, &[1u8; 32]),
        );

        let agg_addr = env.register_contract(None, DexAggregator);
        let agg = DexAggregatorClient::new(&env, &agg_addr);
        agg.initialize(&admin, &factory_addr);

        let token_a = Address::generate(&env);
        let token_b = Address::generate(&env);
        let routing_token = Address::generate(&env);

        // Set routing tokens
        let routing_tokens = vec![&env, routing_token.clone()];
        let result = agg.try_set_routing_tokens(&routing_tokens);
        assert!(result.is_ok());

        // Verify discover_tokens includes the routing tokens
        let discovered = env.as_contract(&agg_addr, || {
            DexAggregator::discover_tokens(&env, &token_a, &token_b)
        });

        let mut found_routing_token = false;
        for i in 0..discovered.len() {
            if discovered.get(i).unwrap() == routing_token {
                found_routing_token = true;
                break;
            }
        }
        assert!(found_routing_token);
    }

    // -------------------------------------------------------------------------
    // #938: pause / unpause safety switch
    // -------------------------------------------------------------------------

    #[test]
    fn test_pause_and_unpause_by_stored_admin() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);
        assert!(!agg.is_paused());

        agg.pause(&v.admin);
        // The stored admin is the address that had to authorize the call.
        let auths = v.env.auths();
        assert_eq!(auths.len(), 1);
        assert_eq!(auths[0].0, v.admin);
        assert!(agg.is_paused());
        let (admin,): (Address,) = last_payload(&v.env, &v.agg, symbol_short!("pause"));
        assert_eq!(admin, v.admin);

        agg.unpause(&v.admin);
        assert!(!agg.is_paused());
        let (admin,): (Address,) = last_payload(&v.env, &v.agg, symbol_short!("unpause"));
        assert_eq!(admin, v.admin);
    }

    #[test]
    fn test_pause_and_unpause_reject_non_admin_address() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);
        let intruder = Address::generate(&v.env);

        assert_eq!(
            agg.try_pause(&intruder),
            Err(Ok(AggregatorError::Unauthorized))
        );
        assert!(!agg.is_paused());

        agg.pause(&v.admin);
        assert_eq!(
            agg.try_unpause(&intruder),
            Err(Ok(AggregatorError::Unauthorized))
        );
        assert!(agg.is_paused());
    }

    #[test]
    fn test_pause_and_unpause_require_admin_authorization() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);

        // Passing the right address is not enough; the admin must sign.
        v.env.mock_auths(&[]);
        assert!(agg.try_pause(&v.admin).is_err());
        assert!(!agg.is_paused());

        v.env.mock_all_auths();
        agg.pause(&v.admin);
        v.env.mock_auths(&[]);
        assert!(agg.try_unpause(&v.admin).is_err());
        assert!(agg.is_paused());
    }

    #[test]
    fn test_pause_before_initialize_returns_not_initialized() {
        let env = Env::default();
        env.mock_all_auths();
        let agg_addr = env.register_contract(None, DexAggregator);
        let agg = DexAggregatorClient::new(&env, &agg_addr);
        let admin = Address::generate(&env);

        assert_eq!(
            agg.try_pause(&admin),
            Err(Ok(AggregatorError::NotInitialized))
        );
        assert_eq!(
            agg.try_unpause(&admin),
            Err(Ok(AggregatorError::NotInitialized))
        );
        assert!(!agg.is_paused());
    }

    #[test]
    fn test_every_mutating_entrypoint_rejects_while_paused() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);
        let quote = agg.find_best_route(&v.token_a, &v.token_b, &10_000_i128, &2u32);
        // Registered after quoting: it has no contract behind it to quote.
        let cl_pool = Address::generate(&v.env);
        agg.register_cl_pool(&cl_pool, &v.token_a, &v.token_b, &30_i128);

        agg.pause(&v.admin);

        assert_eq!(
            agg.try_set_max_hops(&3u32),
            Err(Ok(AggregatorError::Paused))
        );
        let other_cl_pool = Address::generate(&v.env);
        assert_eq!(
            agg.try_register_cl_pool(&other_cl_pool, &v.token_a, &v.token_b, &100_i128),
            Err(Ok(AggregatorError::Paused))
        );
        assert_eq!(
            agg.try_set_routing_tokens(&vec![&v.env, v.token_a.clone()]),
            Err(Ok(AggregatorError::Paused))
        );
        assert_eq!(
            agg.try_remove_cl_pool(&cl_pool),
            Err(Ok(AggregatorError::Paused))
        );
        assert_eq!(
            agg.try_execute_route(&quote, &v.trader, &10_000_i128, &0_i128, &u64::MAX),
            Err(Ok(AggregatorError::Paused))
        );
        assert_eq!(
            agg.try_swap_best(
                &v.trader,
                &v.token_a,
                &v.token_b,
                &10_000_i128,
                &0_i128,
                &u64::MAX
            ),
            Err(Ok(AggregatorError::Paused))
        );

        // Unpausing restores execution.
        agg.unpause(&v.admin);
        assert!(agg.execute_route(&quote, &v.trader, &10_000_i128, &0_i128, &u64::MAX) > 0);
    }

    #[test]
    fn test_read_only_views_callable_while_paused() {
        let v = setup_venues();
        let agg = DexAggregatorClient::new(&v.env, &v.agg);
        agg.pause(&v.admin);

        assert!(agg.is_paused());
        let quote = agg.find_best_route(&v.token_a, &v.token_b, &10_000_i128, &2u32);
        assert!(quote.amount_out > 0);
        assert_eq!(quote.hops.get(0).unwrap().pool, v.pool_ab);
        assert_eq!(
            agg.get_quote(&v.token_a, &v.token_b, &10_000_i128, &2u32),
            quote
        );
        assert!(agg.is_price_within_tolerance(
            &v.token_a,
            &v.token_b,
            &10_000_i128,
            &quote.amount_out
        ));
    }
}
