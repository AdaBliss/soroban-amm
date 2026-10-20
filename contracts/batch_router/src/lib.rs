//! Batched AMM operations — execute multiple swaps and liquidity actions atomically
//! in a single transaction to reduce overhead versus separate calls.

#![no_std]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env, Symbol, Vec};

use pool_interfaces::{AmmPoolClient, ConcentratedLiquidityClient, FactoryClient};
use soroban_amm_sdk::emit_versioned_event;

const MIN_TTL: u32 = 172_800;
const BUMP_TO: u32 = 518_400;

#[contracttype]
pub enum DataKey {
    Factory,
}

/// Pool type for distinguishing between AMM and concentrated liquidity pools.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum PoolType {
    Amm,
    Cl,
}

/// Errors returned by [`BatchRouter`] entry points.
///
/// See `docs/error-codes.md` for the full description of each variant.
#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(u32)]
pub enum BatchRouterError {
    AlreadyInitialized = 1,
    EmptyBatch = 2,
    BatchTooLarge = 3,
    DeadlineExpired = 4,
    InvalidAmount = 5,
    PoolNotFound = 6,
    SlippageExceeded = 7,
    /// The pool is paused, so `execute_batch` would reject this op. Surfaced by
    /// `simulate_batch`/`validate_batch` as a typed failure instead of a raw
    /// pool error.
    PoolPaused = 8,
    /// A `simulate_batch` call reached a second concentrated-liquidity op on a
    /// pool an earlier op in the same batch already touched. The simulator
    /// cannot replay CL tick state locally, so it refuses to quote against the
    /// now-stale snapshot rather than return a wrong amount. Split the batch or
    /// put the CL op first.
    UnsimulatableChain = 9,
    /// The AMM swap's output would meet or exceed the output reserve, which the
    /// pool rejects with `AmmError::InsufficientLiquidity`. Mirrors
    /// `amm::swap` (`contracts/amm/src/lib.rs:2083`).
    InsufficientLiquidity = 10,
    /// The first deposit would mint no more than `MINIMUM_LIQUIDITY` shares, all
    /// of which are permanently locked, leaving the provider with none. Mirrors
    /// `amm::add_liquidity`'s `AmmError::InsufficientShares`
    /// (`contracts/amm/src/lib.rs:1633`).
    InsufficientShares = 11,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct SwapOp {
    pub pool: Address,
    pub token_in: Address,
    pub amount_in: i128,
    pub min_out: i128,
    pub pool_kind: PoolType,
    /// Swap direction for concentrated-liquidity venues: `true` swaps token A
    /// for token B (price decreasing). Unused for `PoolType::Amm`.
    pub zero_for_one: bool,
    /// `sqrtPriceX96` limit for concentrated-liquidity venues. `0` means the
    /// pool's own default bound is used. Unused for `PoolType::Amm`.
    pub sqrt_price_limit_x96: u128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct AddLiquidityOp {
    pub pool: Address,
    pub amount_a: i128,
    pub amount_b: i128,
    pub min_shares: i128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct RemoveLiquidityOp {
    pub pool: Address,
    pub shares: i128,
    pub min_a: i128,
    pub min_b: i128,
}

#[contracttype]
#[derive(Clone, Debug)]
pub enum BatchOp {
    /// Swap `amount_in` of `token_in` on `pool` with `min_out` slippage guard.
    Swap(SwapOp),
    /// Add liquidity to `pool`.
    AddLiquidity(AddLiquidityOp),
    /// Remove liquidity from `pool`.
    RemoveLiquidity(RemoveLiquidityOp),
}

/// Result of a single [`BatchOp`], preserving the real output of each leg so
/// callers chaining batch results can do downstream accounting.
///
/// `RemoveLiquidity` carries both token amounts: packing them into one `i128`
/// would be lossy, and returning the shares burned (which the caller already
/// knows) tells them nothing about the tokens actually received.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum BatchOpResult {
    /// Output amount received from the swap.
    Swap(i128),
    /// LP shares minted by adding liquidity.
    AddLiquidity(i128),
    /// Token amounts `(amount_a, amount_b)` returned by removing liquidity.
    RemoveLiquidity(i128, i128),
}

/// Honest accounting of what a batch actually saves, split by call kind.
///
/// `cross_contract_calls` does NOT shrink when ops are batched: each op
/// inside `execute_batch` is still a separate cross-contract call from the
/// router into the target pool. Batching only collapses the *top-level*
/// call the end user (or their wallet) has to submit.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct CallSavingsEstimate {
    /// Top-level calls a caller would submit executing each op as its own transaction.
    pub top_level_calls_individual: u32,
    /// Top-level calls a caller submits using `execute_batch` (always 1).
    pub top_level_calls_batched: u32,
    /// Cross-contract calls from the router into pools — the same whether
    /// batched or not, since every op still invokes its target pool once.
    pub cross_contract_calls: u32,
}

const MAX_BATCH_OPS: u32 = 200;

/// Permanently locked LP shares minted on a pool's first deposit, mirrored from
/// `amm::add_liquidity` (`contracts/amm/src/lib.rs:1626`). The provider receives
/// the geometric-mean shares *minus* this amount, and a first deposit whose
/// shares do not exceed it is rejected.
const MINIMUM_LIQUIDITY: i128 = 1_000;

/// A pool's reserve/share state as tracked locally while chaining a
/// simulated batch, seeded from `get_info()` on first touch and updated
/// in-memory (never on-chain) as later ops in the same batch are simulated.
#[contracttype]
#[derive(Clone)]
struct SimPoolState {
    pool: Address,
    token_a: Address,
    token_b: Address,
    reserve_a: i128,
    reserve_b: i128,
    total_shares: i128,
    fee_bps: i128,
    protocol_fee_bps: i128,
    lp_rebate_bps: i128,
}

#[contract]
pub struct BatchRouter;

#[contractimpl]
impl BatchRouter {
    /// Initialize the router with the factory that tracks all deployed pools.
    pub fn initialize(env: Env, factory: Address) -> Result<(), BatchRouterError> {
        if env.storage().instance().has(&DataKey::Factory) {
            return Err(BatchRouterError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Factory, &factory);
        Ok(())
    }

    /// The maximum number of operations a single batch may contain.
    pub fn max_batch_ops(_env: Env) -> u32 {
        MAX_BATCH_OPS
    }

    /// Execute a sequence of AMM operations atomically.
    ///
    /// All operations share one `deadline` and a single `caller` authorization.
    /// If any step fails the entire batch reverts.
    pub fn execute_batch(
        env: Env,
        caller: Address,
        ops: Vec<BatchOp>,
        deadline: u64,
    ) -> Result<Vec<BatchOpResult>, BatchRouterError> {
        env.storage().instance().extend_ttl(MIN_TTL, BUMP_TO);
        caller.require_auth();
        Self::check_preconditions(&env, &ops, deadline)?;

        let factory: Address = env.storage().instance().get(&DataKey::Factory).unwrap();
        let factory_client = FactoryClient::new(&env, &factory);

        let mut results = Vec::new(&env);
        for i in 0..ops.len() {
            let op = ops.get(i).unwrap();
            let result = Self::execute_op(&env, &caller, &op, deadline, &factory_client)?;

            emit_versioned_event!(
                env,
                (Symbol::new(&env, "batch_op"), caller.clone()),
                (i, Self::op_kind(&env, &op), result.clone())
            );

            results.push_back(result);
        }

        emit_versioned_event!(
            env,
            (Symbol::new(&env, "batch_executed"), caller.clone()),
            (ops.len(),)
        );

        Ok(results)
    }

    /// Read-only walk that quotes each op against current pool state without
    /// executing or requiring auth. An AMM swap's simulated output feeds the
    /// next op's context on the same pool exactly as `execute_batch` would.
    ///
    /// Concentrated-liquidity venues are quoted through the pool's own
    /// `estimate_price_impact`, which reads live tick state. That state cannot
    /// be replayed locally, so a second CL op on a pool an earlier op in the
    /// same batch already touched returns
    /// [`BatchRouterError::UnsimulatableChain`] rather than a stale quote.
    pub fn simulate_batch(
        env: Env,
        ops: Vec<BatchOp>,
    ) -> Result<Vec<BatchOpResult>, BatchRouterError> {
        let factory: Address = env.storage().instance().get(&DataKey::Factory).unwrap();
        let factory_client = FactoryClient::new(&env, &factory);

        let mut pools: Vec<SimPoolState> = Vec::new(&env);
        let mut cl_touched: Vec<Address> = Vec::new(&env);
        let mut results = Vec::new(&env);

        for i in 0..ops.len() {
            let op = ops.get(i).unwrap();
            let result =
                Self::simulate_op(&env, &op, &factory_client, &mut pools, &mut cl_touched)?;
            results.push_back(result);
        }

        Ok(results)
    }

    /// Run every `execute_batch` precondition, plus a full `simulate_batch`
    /// walk, without executing or mutating any state. Lets a caller find out
    /// why a batch would fail before paying for it.
    pub fn validate_batch(
        env: Env,
        ops: Vec<BatchOp>,
        deadline: u64,
    ) -> Result<(), BatchRouterError> {
        Self::check_preconditions(&env, &ops, deadline)?;
        Self::simulate_batch(env, ops)?;
        Ok(())
    }

    /// Estimate how many top-level contract calls a batch saves vs individual txs.
    ///
    /// Returns `(individual_calls, batch_calls)` for off-chain fee comparison.
    #[deprecated(note = "use estimate_call_savings_v2, which also reports cross-contract calls")]
    #[allow(deprecated)]
    pub fn estimate_call_savings(ops_len: u32) -> (u32, u32) {
        (ops_len, 1)
    }

    /// Honest call-savings breakdown. See [`CallSavingsEstimate`] — the
    /// cross-contract call count does not shrink with batching.
    pub fn estimate_call_savings_v2(_env: Env, ops_len: u32) -> CallSavingsEstimate {
        CallSavingsEstimate {
            top_level_calls_individual: ops_len,
            top_level_calls_batched: 1,
            cross_contract_calls: ops_len,
        }
    }

    fn check_preconditions(
        env: &Env,
        ops: &Vec<BatchOp>,
        deadline: u64,
    ) -> Result<(), BatchRouterError> {
        if ops.is_empty() {
            return Err(BatchRouterError::EmptyBatch);
        }
        if ops.len() > MAX_BATCH_OPS {
            return Err(BatchRouterError::BatchTooLarge);
        }
        if env.ledger().timestamp() > deadline {
            return Err(BatchRouterError::DeadlineExpired);
        }
        Ok(())
    }

    fn op_kind(env: &Env, op: &BatchOp) -> Symbol {
        match op {
            BatchOp::Swap(_) => Symbol::new(env, "swap"),
            BatchOp::AddLiquidity(_) => Symbol::new(env, "add_liquidity"),
            BatchOp::RemoveLiquidity(_) => Symbol::new(env, "remove_liquidity"),
        }
    }

    /// Validate that a pool is registered with the factory and matches the expected pool kind.
    fn validate_pool(
        factory_client: &FactoryClient,
        pool: &Address,
        pool_kind: PoolType,
    ) -> Result<(), BatchRouterError> {
        match pool_kind {
            PoolType::Amm => Self::validate_amm_pool(factory_client, pool)?,
            PoolType::Cl => {
                // For CL pools, use the factory's is_cl_pool view
                if !factory_client.is_cl_pool(pool) {
                    return Err(BatchRouterError::PoolNotFound);
                }
            }
        }
        Ok(())
    }

    /// AMM pools are the only ones the factory records in `get_pool_tokens`.
    fn validate_amm_pool(
        factory_client: &FactoryClient,
        pool: &Address,
    ) -> Result<(), BatchRouterError> {
        if factory_client.get_pool_tokens(pool).is_none() {
            return Err(BatchRouterError::PoolNotFound);
        }
        Ok(())
    }

    /// Per-op validation shared by `execute_op` and `simulate_op`.
    ///
    /// Both paths must accept exactly the same batches, so the pool-kind check
    /// and the amount checks live here once instead of being duplicated — the
    /// CL batch acceptance divergence in #1043 came from `simulate_op` doing
    /// its own AMM-only check.
    fn validate_op(factory_client: &FactoryClient, op: &BatchOp) -> Result<(), BatchRouterError> {
        match op {
            BatchOp::Swap(o) => {
                Self::validate_pool(factory_client, &o.pool, o.pool_kind.clone())?;
                if o.amount_in <= 0 {
                    return Err(BatchRouterError::InvalidAmount);
                }
            }
            BatchOp::AddLiquidity(o) => {
                Self::validate_amm_pool(factory_client, &o.pool)?;
                if o.amount_a <= 0 || o.amount_b <= 0 {
                    return Err(BatchRouterError::InvalidAmount);
                }
            }
            BatchOp::RemoveLiquidity(o) => {
                Self::validate_amm_pool(factory_client, &o.pool)?;
                if o.shares <= 0 {
                    return Err(BatchRouterError::InvalidAmount);
                }
            }
        }
        Ok(())
    }

    fn execute_op(
        env: &Env,
        caller: &Address,
        op: &BatchOp,
        deadline: u64,
        factory_client: &FactoryClient,
    ) -> Result<BatchOpResult, BatchRouterError> {
        Self::validate_op(factory_client, op)?;

        match op {
            BatchOp::Swap(o) => {
                let amount_out = match o.pool_kind {
                    PoolType::Amm => AmmPoolClient::new(env, &o.pool).swap(
                        caller,
                        &o.token_in,
                        &o.amount_in,
                        &o.min_out,
                        &deadline,
                    ),
                    PoolType::Cl => ConcentratedLiquidityClient::new(env, &o.pool).swap(
                        caller,
                        &o.zero_for_one,
                        &o.amount_in,
                        &o.sqrt_price_limit_x96,
                        &o.min_out,
                        &deadline,
                    ),
                };

                Ok(BatchOpResult::Swap(amount_out))
            }
            BatchOp::AddLiquidity(o) => {
                let shares = AmmPoolClient::new(env, &o.pool).add_liquidity(
                    caller,
                    &o.amount_a,
                    &o.amount_b,
                    &o.min_shares,
                    &deadline,
                );
                Ok(BatchOpResult::AddLiquidity(shares))
            }
            BatchOp::RemoveLiquidity(o) => {
                let (a, b) = AmmPoolClient::new(env, &o.pool)
                    .remove_liquidity(caller, &o.shares, &o.min_a, &o.min_b, &deadline);
                Ok(BatchOpResult::RemoveLiquidity(a, b))
            }
        }
    }

    fn find_pool(pools: &Vec<SimPoolState>, pool: &Address) -> Option<u32> {
        (0..pools.len()).find(|&i| &pools.get(i).unwrap().pool == pool)
    }

    fn load_pool(env: &Env, pools: &mut Vec<SimPoolState>, pool: &Address) -> SimPoolState {
        if let Some(idx) = Self::find_pool(pools, pool) {
            return pools.get(idx).unwrap();
        }
        let info = AmmPoolClient::new(env, pool).get_info();
        let state = SimPoolState {
            pool: pool.clone(),
            token_a: info.token_a,
            token_b: info.token_b,
            reserve_a: info.reserve_a,
            reserve_b: info.reserve_b,
            total_shares: info.total_shares,
            fee_bps: info.fee_bps,
            protocol_fee_bps: info.protocol_fee_bps,
            lp_rebate_bps: info.lp_rebate_bps,
        };
        pools.push_back(state.clone());
        state
    }

    fn store_pool(pools: &mut Vec<SimPoolState>, state: SimPoolState) {
        if let Some(idx) = Self::find_pool(pools, &state.pool) {
            pools.set(idx, state);
        } else {
            pools.push_back(state);
        }
    }

    fn simulate_op(
        env: &Env,
        op: &BatchOp,
        factory_client: &FactoryClient,
        pools: &mut Vec<SimPoolState>,
        cl_touched: &mut Vec<Address>,
    ) -> Result<BatchOpResult, BatchRouterError> {
        // Same acceptance rules as `execute_op`, so the two can never disagree
        // on which batches are valid (issue #1043).
        Self::validate_op(factory_client, op)?;

        match op {
            BatchOp::Swap(o) => match o.pool_kind {
                PoolType::Amm => Self::simulate_amm_swap(env, o, pools),
                PoolType::Cl => Self::simulate_cl_swap(env, o, cl_touched),
            },
            BatchOp::AddLiquidity(o) => Self::simulate_add_liquidity(env, o, pools),
            BatchOp::RemoveLiquidity(o) => Self::simulate_remove_liquidity(env, o, pools),
        }
    }

    /// Constant-product swap simulation, mirroring `amm::swap`.
    fn simulate_amm_swap(
        env: &Env,
        o: &SwapOp,
        pools: &mut Vec<SimPoolState>,
    ) -> Result<BatchOpResult, BatchRouterError> {
        let mut state = Self::load_pool(env, pools, &o.pool);
        if AmmPoolClient::new(env, &o.pool).is_paused() {
            return Err(BatchRouterError::PoolPaused);
        }
        let (reserve_in, reserve_out, in_is_a) = if o.token_in == state.token_a {
            (state.reserve_a, state.reserve_b, true)
        } else if o.token_in == state.token_b {
            (state.reserve_b, state.reserve_a, false)
        } else {
            return Err(BatchRouterError::InvalidAmount);
        };
        if reserve_in <= 0 || reserve_out <= 0 {
            return Err(BatchRouterError::PoolNotFound);
        }
        // Mirrors amm::swap (contracts/amm/src/lib.rs:2075-2078).
        let amount_in_with_fee = o.amount_in * (10_000 - state.fee_bps);
        let amount_out =
            amount_in_with_fee * reserve_out / (reserve_in * 10_000 + amount_in_with_fee);
        // Same order as amm::swap: slippage first (2080), then the reserve guard (2083).
        if amount_out < o.min_out {
            return Err(BatchRouterError::SlippageExceeded);
        }
        Self::amm_swap_out_guard(amount_out, reserve_out)?;
        // Mirrors amm::swap's reserve credit, which keeps the net protocol fee
        // out of the LP reserves (contracts/amm/src/lib.rs:2098-2124).
        let protocol_fee = if state.protocol_fee_bps > 0 {
            o.amount_in * state.protocol_fee_bps / 10_000
        } else {
            0
        };
        let lp_rebate = if protocol_fee > 0 && state.lp_rebate_bps > 0 {
            protocol_fee * state.lp_rebate_bps / 10_000
        } else {
            0
        };
        let net_protocol_fee = protocol_fee - lp_rebate;
        let credited_in = o.amount_in - net_protocol_fee;
        if in_is_a {
            state.reserve_a += credited_in;
            state.reserve_b -= amount_out;
        } else {
            state.reserve_b += credited_in;
            state.reserve_a -= amount_out;
        }
        Self::store_pool(pools, state);
        Ok(BatchOpResult::Swap(amount_out))
    }

    /// Concentrated-liquidity swap simulation.
    ///
    /// CL tick state cannot be replayed locally, so the pool is quoted through
    /// its own `estimate_price_impact` and a pool touched earlier in this batch
    /// is rejected rather than quoted against its now-stale snapshot.
    fn simulate_cl_swap(
        env: &Env,
        o: &SwapOp,
        cl_touched: &mut Vec<Address>,
    ) -> Result<BatchOpResult, BatchRouterError> {
        if Self::pool_touched(cl_touched, &o.pool) {
            return Err(BatchRouterError::UnsimulatableChain);
        }
        let cl = ConcentratedLiquidityClient::new(env, &o.pool);
        if cl.is_paused() {
            return Err(BatchRouterError::PoolPaused);
        }
        let estimate =
            cl.estimate_price_impact(&o.zero_for_one, &o.amount_in, &o.sqrt_price_limit_x96);
        if estimate.amount_out < o.min_out {
            return Err(BatchRouterError::SlippageExceeded);
        }
        cl_touched.push_back(o.pool.clone());
        Ok(BatchOpResult::Swap(estimate.amount_out))
    }

    /// Add-liquidity simulation, mirroring `amm::add_liquidity`.
    fn simulate_add_liquidity(
        env: &Env,
        o: &AddLiquidityOp,
        pools: &mut Vec<SimPoolState>,
    ) -> Result<BatchOpResult, BatchRouterError> {
        let mut state = Self::load_pool(env, pools, &o.pool);
        if AmmPoolClient::new(env, &o.pool).is_paused() {
            return Err(BatchRouterError::PoolPaused);
        }
        // Mirrors amm::add_liquidity share math (contracts/amm/src/lib.rs:1610-1618).
        let shares = if state.total_shares == 0 {
            Self::isqrt(o.amount_a * o.amount_b)
        } else {
            let shares_a = o.amount_a * state.total_shares / state.reserve_a;
            let shares_b = o.amount_b * state.total_shares / state.reserve_b;
            shares_a.min(shares_b)
        };
        if shares <= 0 {
            return Err(BatchRouterError::InvalidAmount);
        }
        // Mirrors the permanent MINIMUM_LIQUIDITY lock and its rejection on the
        // first deposit (contracts/amm/src/lib.rs:1624-1639). A first deposit
        // that mints no more than the locked minimum hands the provider nothing.
        let shares_to_provider = if state.total_shares == 0 {
            if shares <= MINIMUM_LIQUIDITY {
                return Err(BatchRouterError::InsufficientShares);
            }
            shares - MINIMUM_LIQUIDITY
        } else {
            shares
        };
        if shares_to_provider < o.min_shares {
            return Err(BatchRouterError::SlippageExceeded);
        }
        state.reserve_a += o.amount_a;
        state.reserve_b += o.amount_b;
        // `total_minted` includes the locked minimum (contracts/amm/src/lib.rs:1652).
        state.total_shares += shares;
        Self::store_pool(pools, state);
        Ok(BatchOpResult::AddLiquidity(shares_to_provider))
    }

    /// Remove-liquidity simulation, mirroring `amm::remove_liquidity`.
    fn simulate_remove_liquidity(
        env: &Env,
        o: &RemoveLiquidityOp,
        pools: &mut Vec<SimPoolState>,
    ) -> Result<BatchOpResult, BatchRouterError> {
        let mut state = Self::load_pool(env, pools, &o.pool);
        if AmmPoolClient::new(env, &o.pool).is_paused() {
            return Err(BatchRouterError::PoolPaused);
        }
        if state.total_shares == 0 {
            return Err(BatchRouterError::InvalidAmount);
        }
        // Mirrors amm::remove_liquidity (contracts/amm/src/lib.rs:1747-1748).
        let out_a = o.shares * state.reserve_a / state.total_shares;
        let out_b = o.shares * state.reserve_b / state.total_shares;
        if out_a < o.min_a || out_b < o.min_b {
            return Err(BatchRouterError::SlippageExceeded);
        }
        state.reserve_a -= out_a;
        state.reserve_b -= out_b;
        state.total_shares -= o.shares;
        Self::store_pool(pools, state);
        Ok(BatchOpResult::RemoveLiquidity(out_a, out_b))
    }

    /// Whether `pool` appears in the list of CL pools already quoted this batch.
    fn pool_touched(seen: &Vec<Address>, pool: &Address) -> bool {
        (0..seen.len()).any(|i| &seen.get(i).unwrap() == pool)
    }

    /// Mirror of `amm::swap`'s output-reserve guard
    /// (`contracts/amm/src/lib.rs:2083`): a swap may never pay out the whole
    /// output reserve. The constant-product formula cannot actually reach this
    /// for valid positive reserves, but the pool keeps the guard as defense in
    /// depth and the simulator mirrors it so the two can never disagree.
    fn amm_swap_out_guard(amount_out: i128, reserve_out: i128) -> Result<(), BatchRouterError> {
        if amount_out >= reserve_out {
            return Err(BatchRouterError::InsufficientLiquidity);
        }
        Ok(())
    }

    /// Integer square root (Newton's method), mirroring `AmmPool::sqrt`.
    fn isqrt(n: i128) -> i128 {
        if n <= 0 {
            return 0;
        }
        let mut x = n;
        let mut y = (x + 1) / 2;
        while y < x {
            x = y;
            y = (x + n / x) / 2;
        }
        x
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use amm::AmmPoolClient as AmmContractClient;
    use concentrated_liquidity::ConcentratedLiquidityClient as ClContractClient;
    use factory::{Factory, FactoryClient};
    use soroban_sdk::{
        testutils::{Address as _, Events, Ledger as _},
        token::{StellarAssetClient, TokenClient as StellarTokenClient},
        vec, Env, TryFromVal,
    };

    fn setup_env_and_factory(env: &Env) -> Address {
        let admin = Address::generate(env);
        env.budget().reset_unlimited();
        let amm_wasm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let lp_wasm_hash = env.deployer().upload_contract_wasm(token::WASM);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(env, &factory_addr);
        factory.initialize(&admin, &amm_wasm_hash, &lp_wasm_hash);
        factory_addr
    }

    fn setup_pool(env: &Env, factory_addr: &Address) -> (Address, Address, Address, Address) {
        let admin = Address::generate(env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let factory = FactoryClient::new(env, factory_addr);
        factory.create_pool(&admin, &ta, &tb, &2_i128, &None);
        let pool = factory.get_pool(&ta, &tb).unwrap();
        let lp = factory.get_lp_token(&pool).unwrap();
        let _ = lp;

        let provider = Address::generate(env);
        StellarAssetClient::new(env, &ta).mint(&provider, &2_000_000_i128);
        StellarAssetClient::new(env, &tb).mint(&provider, &2_000_000_i128);
        AmmPoolClient::new(env, &pool).add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        (ta, tb, pool, provider)
    }

    fn deploy_router<'a>(env: &'a Env, factory_addr: &Address) -> BatchRouterClient<'a> {
        let batch_addr = env.register_contract(None, BatchRouter);
        let batch_client = BatchRouterClient::new(env, &batch_addr);
        batch_client.initialize(factory_addr);
        batch_client
    }

    fn deploy_cl_pool(
        env: &Env,
        factory_addr: &Address,
        admin: &Address,
        token_a: &Address,
        token_b: &Address,
    ) -> Address {
        let cl_addr = env.register_contract(None, concentrated_liquidity::ConcentratedLiquidity);
        let cl = concentrated_liquidity::ConcentratedLiquidityClient::new(env, &cl_addr);
        cl.initialize(admin, token_a, token_b, &30_i128, &0_i32, &10_i32);

        let lp = Address::generate(env);
        StellarAssetClient::new(env, token_a).mint(&lp, &100_000_000_i128);
        StellarAssetClient::new(env, token_b).mint(&lp, &100_000_000_i128);
        cl.mint_position(
            &lp,
            &-1_000_i32,
            &1_000_i32,
            &50_000_000_i128,
            &50_000_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        // Register the pool with the factory so `is_cl_pool` finds it, without
        // going through `create_cl_pool` (which requires a deployed wasm hash).
        env.as_contract(factory_addr, || {
            let count: u64 = env
                .storage()
                .instance()
                .get(&factory::DataKey::ClPoolCount)
                .unwrap_or(0);
            env.storage()
                .persistent()
                .set(&factory::DataKey::ClPoolByIndex(count), &cl_addr);
            env.storage()
                .persistent()
                .set(&factory::DataKey::ClPoolRegistered(cl_addr.clone()), &true);
            env.storage()
                .instance()
                .set(&factory::DataKey::ClPoolCount, &(count + 1));
        });

        cl_addr
    }

    #[test]
    fn test_batch_cl_swap_succeeds() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let admin = Address::generate(&env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_pool = deploy_cl_pool(&env, &factory_addr, &admin, &ta, &tb);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        let ops = vec![
            &env,
            BatchOp::Swap(SwapOp {
                pool: cl_pool.clone(),
                token_in: ta.clone(),
                amount_in: 10_000_i128,
                min_out: 0_i128,
                pool_kind: PoolType::Cl,
                zero_for_one: true,
                sqrt_price_limit_x96: 0_u128,
            }),
        ];

        let batch_client = deploy_router(&env, &factory_addr);
        let deadline = env.ledger().timestamp() + 1000;
        let results = batch_client.execute_batch(&trader, &ops, &deadline);

        assert_eq!(results.len(), 1);
        match results.get(0).unwrap() {
            BatchOpResult::Swap(amount_out) => {
                assert!(amount_out > 0);
                let tb_balance = StellarTokenClient::new(&env, &tb).balance(&trader);
                assert_eq!(tb_balance, amount_out);
                let ta_balance = StellarTokenClient::new(&env, &ta).balance(&trader);
                assert_eq!(ta_balance, 90_000_i128);
            }
            other => panic!("expected swap result, got {other:?}"),
        }
    }

    #[test]
    fn test_batch_mixed_amm_and_cl_swaps() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, tb, amm_pool, _) = setup_pool(&env, &factory_addr);

        let admin = Address::generate(&env);
        let tc = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_pool = deploy_cl_pool(&env, &factory_addr, &admin, &tb, &tc);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);
        StellarAssetClient::new(&env, &tb).mint(&trader, &100_000_i128);

        let ops = vec![
            &env,
            BatchOp::Swap(SwapOp {
                pool: amm_pool.clone(),
                token_in: ta.clone(),
                amount_in: 10_000_i128,
                min_out: 0_i128,
                pool_kind: PoolType::Amm,
                zero_for_one: false,
                sqrt_price_limit_x96: 0_u128,
            }),
            BatchOp::Swap(SwapOp {
                pool: cl_pool.clone(),
                token_in: tb.clone(),
                amount_in: 5_000_i128,
                min_out: 0_i128,
                pool_kind: PoolType::Cl,
                zero_for_one: true,
                sqrt_price_limit_x96: 0_u128,
            }),
        ];

        let batch_client = deploy_router(&env, &factory_addr);
        let deadline = env.ledger().timestamp() + 1000;
        let results = batch_client.execute_batch(&trader, &ops, &deadline);

        assert_eq!(results.len(), 2);
        match results.get(0).unwrap() {
            BatchOpResult::Swap(out) => assert!(out > 0),
            other => panic!("expected swap result, got {other:?}"),
        }
        match results.get(1).unwrap() {
            BatchOpResult::Swap(out) => assert!(out > 0),
            other => panic!("expected swap result, got {other:?}"),
        }
    }

    #[test]
    fn test_batch_cl_swap_unrecognized_pool_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let admin = Address::generate(&env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        let unregistered_pool = Address::generate(&env);

        let ops = vec![
            &env,
            BatchOp::Swap(SwapOp {
                pool: unregistered_pool.clone(),
                token_in: ta.clone(),
                amount_in: 10_000_i128,
                min_out: 0_i128,
                pool_kind: PoolType::Cl,
                zero_for_one: true,
                sqrt_price_limit_x96: 0_u128,
            }),
        ];

        let batch_client = deploy_router(&env, &factory_addr);
        let deadline = env.ledger().timestamp() + 1000;
        let result = batch_client.try_execute_batch(&trader, &ops, &deadline);

        assert_eq!(result, Err(Ok(BatchRouterError::PoolNotFound)));
    }

    #[test]
    fn test_batch_cl_swap_zero_for_one_direction_respected() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let admin = Address::generate(&env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_pool = deploy_cl_pool(&env, &factory_addr, &admin, &ta, &tb);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);
        StellarAssetClient::new(&env, &tb).mint(&trader, &100_000_i128);

        let ops1 = vec![
            &env,
            BatchOp::Swap(SwapOp {
                pool: cl_pool.clone(),
                token_in: ta.clone(),
                amount_in: 5_000_i128,
                min_out: 0_i128,
                pool_kind: PoolType::Cl,
                zero_for_one: true,
                sqrt_price_limit_x96: 0_u128,
            }),
        ];

        let batch_client = deploy_router(&env, &factory_addr);
        let deadline = env.ledger().timestamp() + 1000;
        let results1 = batch_client.execute_batch(&trader, &ops1, &deadline);

        let tb_out1 = match results1.get(0).unwrap() {
            BatchOpResult::Swap(out) => out,
            other => panic!("expected swap result, got {other:?}"),
        };
        assert!(tb_out1 > 0);

        let ops2 = vec![
            &env,
            BatchOp::Swap(SwapOp {
                pool: cl_pool.clone(),
                token_in: tb.clone(),
                amount_in: 3_000_i128,
                min_out: 0_i128,
                pool_kind: PoolType::Cl,
                zero_for_one: false,
                sqrt_price_limit_x96: 0_u128,
            }),
        ];

        let results2 = batch_client.execute_batch(&trader, &ops2, &deadline);

        let ta_out2 = match results2.get(0).unwrap() {
            BatchOpResult::Swap(out) => out,
            other => panic!("expected swap result, got {other:?}"),
        };
        assert!(ta_out2 > 0);
    }

    #[test]
    fn test_batch_all_cl_swaps() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let admin = Address::generate(&env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_pool1 = deploy_cl_pool(&env, &factory_addr, &admin, &ta, &tb);

        let tc = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_pool2 = deploy_cl_pool(&env, &factory_addr, &admin, &tb, &tc);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &200_000_i128);

        let ops = vec![
            &env,
            BatchOp::Swap(SwapOp {
                pool: cl_pool1.clone(),
                token_in: ta.clone(),
                amount_in: 50_000_i128,
                min_out: 0_i128,
                pool_kind: PoolType::Cl,
                zero_for_one: true,
                sqrt_price_limit_x96: 0_u128,
            }),
            BatchOp::Swap(SwapOp {
                pool: cl_pool2.clone(),
                token_in: tb.clone(),
                amount_in: 25_000_i128,
                min_out: 0_i128,
                pool_kind: PoolType::Cl,
                zero_for_one: true,
                sqrt_price_limit_x96: 0_u128,
            }),
        ];

        let batch_client = deploy_router(&env, &factory_addr);
        let deadline = env.ledger().timestamp() + 1000;
        let results = batch_client.execute_batch(&trader, &ops, &deadline);

        assert_eq!(results.len(), 2);
        match results.get(0).unwrap() {
            BatchOpResult::Swap(out) => assert!(out > 0),
            other => panic!("expected swap result, got {other:?}"),
        }
        match results.get(1).unwrap() {
            BatchOpResult::Swap(out) => assert!(out > 0),
            other => panic!("expected swap result, got {other:?}"),
        }
    }

    #[test]
    fn test_batch_atomic_revert_on_cl_slippage() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let admin = Address::generate(&env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_pool = deploy_cl_pool(&env, &factory_addr, &admin, &ta, &tb);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);
        StellarAssetClient::new(&env, &tb).mint(&trader, &100_000_i128);

        let ops = vec![
            &env,
            BatchOp::Swap(SwapOp {
                pool: cl_pool.clone(),
                token_in: ta.clone(),
                amount_in: 10_000_i128,
                min_out: 0_i128,
                pool_kind: PoolType::Cl,
                zero_for_one: true,
                sqrt_price_limit_x96: 0_u128,
            }),
            BatchOp::Swap(SwapOp {
                pool: cl_pool.clone(),
                token_in: tb.clone(),
                amount_in: 5_000_i128,
                min_out: 1_000_000_000_i128,
                pool_kind: PoolType::Cl,
                zero_for_one: false,
                sqrt_price_limit_x96: 0_u128,
            }),
        ];

        let batch_client = deploy_router(&env, &factory_addr);
        let deadline = env.ledger().timestamp() + 1000;
        let result = batch_client.try_execute_batch(&trader, &ops, &deadline);

        assert!(result.is_err());
    }

    #[test]
    fn test_batch_exceeds_max_ops_with_mixed_types() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, tb, amm_pool, _) = setup_pool(&env, &factory_addr);

        let admin = Address::generate(&env);
        let tc = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_pool = deploy_cl_pool(&env, &factory_addr, &admin, &tb, &tc);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &500_000_i128);

        let mut ops = Vec::new(&env);
        for i in 0..201 {
            if i % 2 == 0 {
                ops.push_back(BatchOp::Swap(SwapOp {
                    pool: amm_pool.clone(),
                    token_in: ta.clone(),
                    amount_in: 100_i128,
                    min_out: 0_i128,
                    pool_kind: PoolType::Amm,
                    zero_for_one: false,
                    sqrt_price_limit_x96: 0_u128,
                }));
            } else {
                ops.push_back(BatchOp::Swap(SwapOp {
                    pool: cl_pool.clone(),
                    token_in: tb.clone(),
                    amount_in: 100_i128,
                    min_out: 0_i128,
                    pool_kind: PoolType::Cl,
                    zero_for_one: true,
                    sqrt_price_limit_x96: 0_u128,
                }));
            }
        }

        let batch_client = deploy_router(&env, &factory_addr);
        let deadline = env.ledger().timestamp() + 1000;
        let result = batch_client.try_execute_batch(&trader, &ops, &deadline);

        assert_eq!(result, Err(Ok(BatchRouterError::BatchTooLarge)));
    }

    #[test]
    fn test_batch_executed_emits_versioned_event_with_schema_version() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, tb, pool, _) = setup_pool(&env, &factory_addr);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);
        StellarAssetClient::new(&env, &tb).mint(&trader, &100_000_i128);

        let ops = vec![
            &env,
            BatchOp::Swap(SwapOp {
                pool: pool.clone(),
                token_in: ta.clone(),
                amount_in: 10_000_i128,
                min_out: 0_i128,
                pool_kind: PoolType::Amm,
                zero_for_one: false,
                sqrt_price_limit_x96: 0_u128,
            }),
            BatchOp::Swap(SwapOp {
                pool: pool.clone(),
                token_in: tb.clone(),
                amount_in: 5_000_i128,
                min_out: 0_i128,
                pool_kind: PoolType::Amm,
                zero_for_one: false,
                sqrt_price_limit_x96: 0_u128,
            }),
        ];

        let batch_client = deploy_router(&env, &factory_addr);
        let deadline = env.ledger().timestamp() + 1000;
        let _results = batch_client.execute_batch(&trader, &ops, &deadline);

        // Read all events and find the batch_executed event
        let events = env.events().all();
        let batch_executed_events: std::vec::Vec<_> = events
            .iter()
            .filter(|event| {
                if let Some(topic_val) = event.1.get(0) {
                    if let Ok(topic) = Symbol::try_from_val(&env, &topic_val) {
                        topic == Symbol::new(&env, "batch_executed")
                    } else {
                        false
                    }
                } else {
                    false
                }
            })
            .collect();

        assert!(
            !batch_executed_events.is_empty(),
            "batch_executed event must be emitted"
        );

        // Last event should be batch_executed with version prefix
        let event = batch_executed_events.last().unwrap();
        let (version, (ops_len,)): (u32, (u32,)) =
            <(u32, (u32,))>::try_from_val(&env, &event.2).expect("must decode as (u32, (u32,))");
        assert_eq!(
            version,
            soroban_amm_sdk::EVENT_SCHEMA_VERSION,
            "event must have correct schema version"
        );
        assert_eq!(ops_len, 2, "event must record correct operation count");
    }

    // ───────────────────────── Issue #1043 ─────────────────────────
    // `simulate_batch`/`validate_batch` must accept every batch `execute_batch`
    // accepts and return byte-identical results.

    /// Factory whose admin is returned, so tests can call pool-admin entry
    /// points (`set_protocol_fee`) on pools it creates.
    fn setup_factory_with_admin(env: &Env) -> (Address, Address) {
        let admin = Address::generate(env);
        env.budget().reset_unlimited();
        let amm_wasm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let lp_wasm_hash = env.deployer().upload_contract_wasm(token::WASM);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(env, &factory_addr);
        factory.initialize(&admin, &amm_wasm_hash, &lp_wasm_hash);
        (factory_addr, admin)
    }

    /// Create an AMM pool with an explicit fee (bypassing the fee-tier table),
    /// returning `(token_a, token_b, pool)` in canonical order.
    fn create_fee_pool(
        env: &Env,
        factory_addr: &Address,
        fee_bps: i128,
    ) -> (Address, Address, Address) {
        let creator = Address::generate(env);
        let ta = env
            .register_stellar_asset_contract_v2(creator.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(creator.clone())
            .address();
        let factory = FactoryClient::new(env, factory_addr);
        let (pool, _gov) = factory.create_pool_with_fee_bps(&creator, &ta, &tb, &fee_bps, &None);
        let (token_a, token_b) = factory.get_pool_tokens(&pool).unwrap();
        (token_a, token_b, pool)
    }

    fn fund(env: &Env, token: &Address, to: &Address, amount: i128) {
        StellarAssetClient::new(env, token).mint(to, &amount);
    }

    fn swap_op(
        pool: &Address,
        token_in: &Address,
        amount_in: i128,
        kind: PoolType,
        zero_for_one: bool,
    ) -> BatchOp {
        BatchOp::Swap(SwapOp {
            pool: pool.clone(),
            token_in: token_in.clone(),
            amount_in,
            min_out: 0_i128,
            pool_kind: kind,
            zero_for_one,
            sqrt_price_limit_x96: 0_u128,
        })
    }

    fn add_op(pool: &Address, amount_a: i128, amount_b: i128, min_shares: i128) -> BatchOp {
        BatchOp::AddLiquidity(AddLiquidityOp {
            pool: pool.clone(),
            amount_a,
            amount_b,
            min_shares,
        })
    }

    fn remove_op(pool: &Address, shares: i128) -> BatchOp {
        BatchOp::RemoveLiquidity(RemoveLiquidityOp {
            pool: pool.clone(),
            shares,
            min_a: 0_i128,
            min_b: 0_i128,
        })
    }

    /// The core #1043 invariant: simulation and execution agree exactly.
    fn assert_sim_matches_execute(
        env: &Env,
        client: &BatchRouterClient,
        caller: &Address,
        ops: &Vec<BatchOp>,
    ) {
        let deadline = env.ledger().timestamp() + 1_000;
        let simulated = client.simulate_batch(ops);
        let executed = client.execute_batch(caller, ops, &deadline);
        assert_eq!(
            simulated, executed,
            "simulate_batch must equal execute_batch"
        );
    }

    // ── Equivalence: ≥8 batch shapes ──────────────────────────────

    #[test]
    fn test_sim_matches_execute_single_amm_swap() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, tb, pool, _) = setup_pool(&env, &factory_addr);
        let trader = Address::generate(&env);
        fund(&env, &ta, &trader, 100_000);
        fund(&env, &tb, &trader, 100_000);

        let client = deploy_router(&env, &factory_addr);
        let ops = vec![&env, swap_op(&pool, &ta, 10_000, PoolType::Amm, false)];
        assert_sim_matches_execute(&env, &client, &trader, &ops);
    }

    #[test]
    fn test_sim_matches_execute_chained_amm_swaps() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, tb, pool, _) = setup_pool(&env, &factory_addr);
        let trader = Address::generate(&env);
        fund(&env, &ta, &trader, 100_000);
        fund(&env, &tb, &trader, 100_000);

        let client = deploy_router(&env, &factory_addr);
        let ops = vec![
            &env,
            swap_op(&pool, &ta, 10_000, PoolType::Amm, false),
            swap_op(&pool, &tb, 7_000, PoolType::Amm, true),
        ];
        // A correct simulator must chain the first swap's reserve changes into
        // the second leg rather than quote both against the same snapshot.
        assert_sim_matches_execute(&env, &client, &trader, &ops);
    }

    #[test]
    fn test_sim_matches_execute_add_liquidity() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, tb, pool, provider) = setup_pool(&env, &factory_addr);
        fund(&env, &ta, &provider, 50_000);
        fund(&env, &tb, &provider, 50_000);

        let client = deploy_router(&env, &factory_addr);
        let ops = vec![&env, add_op(&pool, 20_000, 20_000, 0)];
        assert_sim_matches_execute(&env, &client, &provider, &ops);
    }

    #[test]
    fn test_sim_matches_execute_remove_liquidity() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (_, _, pool, provider) = setup_pool(&env, &factory_addr);

        let client = deploy_router(&env, &factory_addr);
        let ops = vec![&env, remove_op(&pool, 100_000)];
        assert_sim_matches_execute(&env, &client, &provider, &ops);
    }

    #[test]
    fn test_sim_matches_execute_first_deposit() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, tb, pool) = create_fee_pool(&env, &factory_addr, 30);
        let provider = Address::generate(&env);
        fund(&env, &ta, &provider, 2_000_000);
        fund(&env, &tb, &provider, 2_000_000);

        let client = deploy_router(&env, &factory_addr);
        let ops = vec![&env, add_op(&pool, 2_000_000, 2_000_000, 0)];
        assert_sim_matches_execute(&env, &client, &provider, &ops);
    }

    #[test]
    fn test_sim_matches_execute_mixed_amm_and_cl() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, tb, amm_pool, _) = setup_pool(&env, &factory_addr);
        let admin = Address::generate(&env);
        let tc = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_pool = deploy_cl_pool(&env, &factory_addr, &admin, &tb, &tc);

        let trader = Address::generate(&env);
        fund(&env, &ta, &trader, 100_000);
        fund(&env, &tb, &trader, 100_000);

        let client = deploy_router(&env, &factory_addr);
        let ops = vec![
            &env,
            swap_op(&amm_pool, &ta, 10_000, PoolType::Amm, false),
            swap_op(&cl_pool, &tb, 5_000, PoolType::Cl, true),
        ];
        assert_sim_matches_execute(&env, &client, &trader, &ops);
    }

    #[test]
    fn test_sim_matches_execute_protocol_fee_chained_swaps() {
        let env = Env::default();
        env.mock_all_auths();
        let (factory_addr, admin) = setup_factory_with_admin(&env);
        let (ta, tb, pool) = create_fee_pool(&env, &factory_addr, 30);
        let provider = Address::generate(&env);
        fund(&env, &ta, &provider, 2_000_000);
        fund(&env, &tb, &provider, 2_000_000);
        AmmPoolClient::new(&env, &pool).add_liquidity(
            &provider,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let recipient = Address::generate(&env);
        AmmContractClient::new(&env, &pool).set_protocol_fee(&admin, &recipient, &10_i128);
        AmmContractClient::new(&env, &pool).set_lp_rebate(&admin, &5_i128);

        let trader = Address::generate(&env);
        fund(&env, &ta, &trader, 100_000);
        fund(&env, &tb, &trader, 100_000);

        let client = deploy_router(&env, &factory_addr);
        let ops = vec![
            &env,
            swap_op(&pool, &ta, 10_000, PoolType::Amm, false),
            swap_op(&pool, &tb, 7_000, PoolType::Amm, true),
        ];
        // With a protocol fee, the reserve credited to the pool is net of the
        // fee; a naive simulator would diverge on the second leg.
        assert_sim_matches_execute(&env, &client, &trader, &ops);
    }

    #[test]
    fn test_sim_matches_execute_swap_then_remove_same_pool() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, tb, pool, provider) = setup_pool(&env, &factory_addr);
        fund(&env, &ta, &provider, 50_000);
        fund(&env, &tb, &provider, 50_000);

        let client = deploy_router(&env, &factory_addr);
        let ops = vec![
            &env,
            swap_op(&pool, &ta, 10_000, PoolType::Amm, false),
            remove_op(&pool, 100_000),
        ];
        assert_sim_matches_execute(&env, &client, &provider, &ops);
    }

    #[test]
    fn test_sim_matches_execute_swap_then_add_same_pool() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, tb, pool, provider) = setup_pool(&env, &factory_addr);
        fund(&env, &ta, &provider, 50_000);
        fund(&env, &tb, &provider, 50_000);

        let client = deploy_router(&env, &factory_addr);
        let ops = vec![
            &env,
            swap_op(&pool, &ta, 10_000, PoolType::Amm, false),
            add_op(&pool, 20_000, 20_000, 0),
        ];
        assert_sim_matches_execute(&env, &client, &provider, &ops);
    }

    // ── Regression: CL batches must be accepted (bug #1) ──────────

    #[test]
    fn test_validate_batch_accepts_cl_swap() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let admin = Address::generate(&env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_pool = deploy_cl_pool(&env, &factory_addr, &admin, &ta, &tb);
        let trader = Address::generate(&env);
        fund(&env, &ta, &trader, 100_000);

        let client = deploy_router(&env, &factory_addr);
        let ops = vec![&env, swap_op(&cl_pool, &ta, 5_000, PoolType::Cl, true)];
        let deadline = env.ledger().timestamp() + 1_000;
        // On `main` this returned PoolNotFound for every CL pool.
        assert_eq!(client.try_validate_batch(&ops, &deadline), Ok(Ok(())));
    }

    #[test]
    fn test_validate_batch_accepts_mixed_cl_and_amm() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, tb, amm_pool, _) = setup_pool(&env, &factory_addr);
        let admin = Address::generate(&env);
        let tc = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_pool = deploy_cl_pool(&env, &factory_addr, &admin, &tb, &tc);
        let trader = Address::generate(&env);
        fund(&env, &ta, &trader, 100_000);
        fund(&env, &tb, &trader, 100_000);

        let client = deploy_router(&env, &factory_addr);
        let ops = vec![
            &env,
            swap_op(&amm_pool, &ta, 10_000, PoolType::Amm, false),
            swap_op(&cl_pool, &tb, 5_000, PoolType::Cl, true),
        ];
        let deadline = env.ledger().timestamp() + 1_000;
        assert_eq!(client.try_validate_batch(&ops, &deadline), Ok(Ok(())));
    }

    // ── Regression: first-deposit share accounting (bug #2) ───────

    #[test]
    fn test_validate_batch_rejects_first_deposit_min_shares_in_gap() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, tb, pool) = create_fee_pool(&env, &factory_addr, 30);
        let provider = Address::generate(&env);
        fund(&env, &ta, &provider, 2_000_000);
        fund(&env, &tb, &provider, 2_000_000);

        // shares = 2_000_000; provider receives 1_999_000. A `min_shares` in the
        // 1000-share lock gap must be rejected, not silently satisfied.
        let client = deploy_router(&env, &factory_addr);
        let ops = vec![&env, add_op(&pool, 2_000_000, 2_000_000, 1_999_500)];
        let deadline = env.ledger().timestamp() + 1_000;
        assert_eq!(
            client.try_validate_batch(&ops, &deadline),
            Err(Ok(BatchRouterError::SlippageExceeded))
        );
    }

    #[test]
    fn test_validate_batch_rejects_first_deposit_at_minimum() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, tb, pool) = create_fee_pool(&env, &factory_addr, 30);
        let provider = Address::generate(&env);
        fund(&env, &ta, &provider, 1_000);
        fund(&env, &tb, &provider, 1_000);

        // sqrt(1_000 * 1_000) == MINIMUM_LIQUIDITY: nothing is left for the provider.
        let client = deploy_router(&env, &factory_addr);
        let ops = vec![&env, add_op(&pool, 1_000, 1_000, 0)];
        let deadline = env.ledger().timestamp() + 1_000;
        assert_eq!(
            client.try_validate_batch(&ops, &deadline),
            Err(Ok(BatchRouterError::InsufficientShares))
        );
    }

    // ── Paused pools (bug #4) ─────────────────────────────────────

    #[test]
    fn test_validate_batch_rejects_paused_amm_pool() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, tb, pool, _) = setup_pool(&env, &factory_addr);
        AmmContractClient::new(&env, &pool).pause();
        let trader = Address::generate(&env);
        fund(&env, &ta, &trader, 100_000);
        fund(&env, &tb, &trader, 100_000);

        let client = deploy_router(&env, &factory_addr);
        let ops = vec![&env, swap_op(&pool, &ta, 10_000, PoolType::Amm, false)];
        let deadline = env.ledger().timestamp() + 1_000;
        assert_eq!(
            client.try_validate_batch(&ops, &deadline),
            Err(Ok(BatchRouterError::PoolPaused))
        );
    }

    #[test]
    fn test_validate_batch_rejects_paused_cl_pool() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let admin = Address::generate(&env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_pool = deploy_cl_pool(&env, &factory_addr, &admin, &ta, &tb);
        ClContractClient::new(&env, &cl_pool).pause(&admin);
        let trader = Address::generate(&env);
        fund(&env, &ta, &trader, 100_000);

        let client = deploy_router(&env, &factory_addr);
        let ops = vec![&env, swap_op(&cl_pool, &ta, 5_000, PoolType::Cl, true)];
        let deadline = env.ledger().timestamp() + 1_000;
        assert_eq!(
            client.try_validate_batch(&ops, &deadline),
            Err(Ok(BatchRouterError::PoolPaused))
        );
    }

    // ── Unsimulatable CL chains ───────────────────────────────────

    #[test]
    fn test_simulate_rejects_second_cl_op_on_touched_pool() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let admin = Address::generate(&env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_pool = deploy_cl_pool(&env, &factory_addr, &admin, &ta, &tb);
        let trader = Address::generate(&env);
        fund(&env, &ta, &trader, 100_000);
        fund(&env, &tb, &trader, 100_000);

        let client = deploy_router(&env, &factory_addr);
        let ops = vec![
            &env,
            swap_op(&cl_pool, &ta, 5_000, PoolType::Cl, true),
            swap_op(&cl_pool, &tb, 5_000, PoolType::Cl, true),
        ];
        assert_eq!(
            client.try_simulate_batch(&ops),
            Err(Ok(BatchRouterError::UnsimulatableChain))
        );
    }

    // ── Full per-variant coverage ─────────────────────────────────

    #[test]
    fn test_amm_swap_out_guard_rejects_full_reserve() {
        assert_eq!(
            BatchRouter::amm_swap_out_guard(1_000, 1_000),
            Err(BatchRouterError::InsufficientLiquidity)
        );
        assert_eq!(
            BatchRouter::amm_swap_out_guard(1_001, 1_000),
            Err(BatchRouterError::InsufficientLiquidity)
        );
        assert_eq!(BatchRouter::amm_swap_out_guard(999, 1_000), Ok(()));
    }

    #[test]
    fn test_validate_batch_rejects_empty_batch() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let client = deploy_router(&env, &factory_addr);
        let ops: Vec<BatchOp> = Vec::new(&env);
        let deadline = env.ledger().timestamp() + 1_000;
        assert_eq!(
            client.try_validate_batch(&ops, &deadline),
            Err(Ok(BatchRouterError::EmptyBatch))
        );
    }

    #[test]
    fn test_validate_batch_rejects_oversized_batch() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, _, pool, _) = setup_pool(&env, &factory_addr);
        let client = deploy_router(&env, &factory_addr);
        let mut ops: Vec<BatchOp> = Vec::new(&env);
        for _ in 0..(MAX_BATCH_OPS + 1) {
            ops.push_back(swap_op(&pool, &ta, 1_000, PoolType::Amm, false));
        }
        let deadline = env.ledger().timestamp() + 1_000;
        assert_eq!(
            client.try_validate_batch(&ops, &deadline),
            Err(Ok(BatchRouterError::BatchTooLarge))
        );
    }

    #[test]
    fn test_validate_batch_rejects_expired_deadline() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, _, pool, _) = setup_pool(&env, &factory_addr);
        env.ledger().set_timestamp(1_000);
        let client = deploy_router(&env, &factory_addr);
        let ops = vec![&env, swap_op(&pool, &ta, 10_000, PoolType::Amm, false)];
        assert_eq!(
            client.try_validate_batch(&ops, &999),
            Err(Ok(BatchRouterError::DeadlineExpired))
        );
    }

    #[test]
    fn test_validate_batch_rejects_zero_amount() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, _, pool, _) = setup_pool(&env, &factory_addr);
        let client = deploy_router(&env, &factory_addr);
        let ops = vec![&env, swap_op(&pool, &ta, 0, PoolType::Amm, false)];
        let deadline = env.ledger().timestamp() + 1_000;
        assert_eq!(
            client.try_validate_batch(&ops, &deadline),
            Err(Ok(BatchRouterError::InvalidAmount))
        );
    }

    #[test]
    fn test_validate_batch_rejects_unregistered_pool() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, _, _, _) = setup_pool(&env, &factory_addr);
        let stranger = Address::generate(&env);
        let client = deploy_router(&env, &factory_addr);

        let deadline = env.ledger().timestamp() + 1_000;
        let amm_ops = vec![&env, swap_op(&stranger, &ta, 1_000, PoolType::Amm, false)];
        assert_eq!(
            client.try_validate_batch(&amm_ops, &deadline),
            Err(Ok(BatchRouterError::PoolNotFound))
        );
        let cl_ops = vec![&env, swap_op(&stranger, &ta, 1_000, PoolType::Cl, true)];
        assert_eq!(
            client.try_validate_batch(&cl_ops, &deadline),
            Err(Ok(BatchRouterError::PoolNotFound))
        );
    }

    #[test]
    fn test_validate_batch_rejects_excessive_slippage() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (ta, _, pool, _) = setup_pool(&env, &factory_addr);
        let client = deploy_router(&env, &factory_addr);
        let ops = vec![
            &env,
            BatchOp::Swap(SwapOp {
                pool: pool.clone(),
                token_in: ta.clone(),
                amount_in: 10_000_i128,
                min_out: i128::MAX,
                pool_kind: PoolType::Amm,
                zero_for_one: false,
                sqrt_price_limit_x96: 0_u128,
            }),
        ];
        let deadline = env.ledger().timestamp() + 1_000;
        assert_eq!(
            client.try_validate_batch(&ops, &deadline),
            Err(Ok(BatchRouterError::SlippageExceeded))
        );
    }

    #[test]
    fn test_validate_batch_rejects_remove_with_no_shares() {
        let env = Env::default();
        env.mock_all_auths();
        let factory_addr = setup_env_and_factory(&env);
        let (_, _, pool, _) = setup_pool(&env, &factory_addr);
        let client = deploy_router(&env, &factory_addr);
        let ops = vec![&env, remove_op(&pool, 0)];
        let deadline = env.ledger().timestamp() + 1_000;
        assert_eq!(
            client.try_validate_batch(&ops, &deadline),
            Err(Ok(BatchRouterError::InvalidAmount))
        );
    }
}
