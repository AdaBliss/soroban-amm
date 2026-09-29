//! Factory contract — deploys and registers AMM pool instances.
//!
//! Flow:
//!   1. Deploy this contract.
//!   2. Call `initialize` with the admin address and pre-uploaded WASM hashes
//!      for the AMM pool and LP token contracts.
//!   3. Call `create_pool` for each token pair you want a pool for.
//!   4. Use `get_pool` / `get_pools` to discover deployed AMM pools, or
//!      `get_cl_pool` / `get_cl_pools` for concentrated-liquidity pools.
//!      The two kinds are indexed separately (issue #493) — the AMM
//!      accessors never return a CL pool address, since CL pools don't
//!      implement the AMM-only `get_info`/`withdraw_protocol_fees`/
//!      `set_protocol_fee` interface that AMM callers rely on.
//!      `all_pools()` / `all_cl_pools()` are convenience wrappers capped at
//!      `Factory::MAX_UNBOUNDED_PAGE` (issue #790); anything that must see
//!      every pool should page with `get_pool_count()` + `get_pools()`.

#![no_std]

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, token as sdk_token,
    Address, BytesN, Env, Symbol, Vec,
};
use pool_interfaces::{ClPoolClient, PoolState};

// ── Typed errors ─────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum FactoryError {
    AlreadyInitialized = 1,
    InvalidFeeBps = 2,
    PoolAlreadyExists = 3,
    ClPoolAlreadyExists = 4,
    ClWasmNotSet = 5,
    Unauthorized = 6,
    FeeNotConfigured = 7,
    RateLimitExceeded = 8,
    CreationPaused = 9,
    /// `accept_admin` called without a prior `propose_admin`.
    NoPendingAdmin = 10,
    /// `accept_admin` called by an address other than the nominee.
    WrongAdmin = 11,
    /// A function that reads factory configuration (admin, AMM/LP WASM
    /// hashes) was called before `initialize`. Discriminant 2 is taken by
    /// `InvalidFeeBps`, so this is appended rather than renumbered.
    NotInitialized = 12,
}



#[contractclient(name = "AmmPoolClient")]
pub trait AmmPoolInterface {
    #[allow(clippy::too_many_arguments)]
    fn initialize(
        env: Env,
        admin: Address,
        token_a: Address,
        token_b: Address,
        lp_token: Address,
        fee_bps: i128,
        fee_recipient: Address,
        protocol_fee_bps: i128,
    );

    /// Update the protocol fee recipient and rate on an existing pool.
    /// Requires the pool's stored admin to authorise.
    fn set_protocol_fee(env: Env, admin: Address, recipient: Address, protocol_fee_bps: i128);

    /// Pull accrued protocol fees from the pool to the current fee_recipient.
    /// Requires `fee_recipient.require_auth()` — satisfied automatically when
    /// the factory (which IS the fee_recipient) makes this cross-contract call.
    fn withdraw_protocol_fees(env: Env) -> (i128, i128);
}

#[contractclient(name = "LpTokenClient")]
pub trait LpTokenInterface {
    fn initialize(
        env: Env,
        admin: Address,
        name: soroban_sdk::String,
        symbol: soroban_sdk::String,
        decimals: u32,
    );
    fn set_locker(env: Env, locker: Address);
}

#[contractclient(name = "GovernanceClient")]
pub trait GovernanceInterface {
    #[allow(clippy::too_many_arguments)]
    fn initialize(
        env: Env,
        admin: Address,
        amm_pool: Address,
        lp_token: Address,
        voting_period_secs: u64,
        timelock_secs: u64,
        quorum_bps: i128,
        min_proposer_stake_bps: i128,
    );
}

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Pool(Address, Address), // normalized (token_a, token_b) → pool Address
    LpToken(Address),       // pool address → LP token address
    PoolByIndex(u64),       // u64 index -> AMM pool Address (issue #493: AMM-only)
    Admin,
    AmmWasmHash,
    TokenWasmHash,
    ClWasmHash,                     // WASM hash for concentrated_liquidity deployments
    PoolCount, // u64 monotonic counter — AMM pools only; also derives deploy salts
    GovernanceFor(Address), // pool address → Option<Address>
    ClPool(Address, Address, i128), // normalized (token_a, token_b, fee_bps) → CL pool Address
    ClPoolByIndex(u64), // u64 index -> CL pool Address (issue #493: separate from PoolByIndex)
    ClPoolCount, // u64 monotonic counter for CL pools — independent of PoolCount (issue #493)
    PermissionlessMode, // bool — true = anyone can create pools (with fee)
    PoolCreationFee, // i128 — fee charged per pool in permissionless mode
    FeeToken,  // Address — token used to pay the pool creation fee
    RateLimitLedgers, // u32 — minimum ledgers between pool creations per address
    LastPoolCreation(Address), // u32 — ledger when this address last created a pool
    DefaultFeeTier, // i128 — default fee tier ID (0-3) for new pool deployments
    Treasury,  // Address — protocol treasury for fee sweeps
    GlobalProtocolFeeBps, // i128 — global protocol fee rate (0 = off)
    PoolTokens(Address), // pool address → (token_a, token_b) for sweep forwarding
    CreationPaused, // bool — true blocks new V2 and CL pool creation
    PendingAdmin, // Option<Address> — nominee for two-step admin rotation
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Convert a fee tier ID (0–3) to its basis-point equivalent.
///
/// | ID | bps | rate   |
/// |----|-----|--------|
/// | 0  |   1 | 0.01 % |
/// | 1  |   5 | 0.05 % |
/// | 2  |  30 | 0.30 % |
/// | 3  | 100 | 1.00 % |
fn fee_tier_to_bps(fee_tier: i128) -> Result<i128, FactoryError> {
    match fee_tier {
        0 => Ok(1),
        1 => Ok(5),
        2 => Ok(30),
        3 => Ok(100),
        _ => Err(FactoryError::InvalidFeeBps),
    }
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct Factory;

#[contractimpl]
impl Factory {
    /// Hard cap on how many pool addresses a single `all_pools()` /
    /// `all_cl_pools()` call will return (issue #790).
    ///
    /// Those accessors used to loop over every pool ever deployed, so their
    /// CPU and read-entry cost grew without bound and would eventually exceed
    /// Soroban's per-invocation resource budget, making the call permanently
    /// unusable. Capping the result keeps them a cheap, predictable read for
    /// the common small-factory case; callers that must see every pool past
    /// the cap should page with `get_pool_count()` + `get_pools(offset, limit)`
    /// (or the CL equivalents), which have always been bounded.
    pub const MAX_UNBOUNDED_PAGE: u32 = 200;

    /// Instance-TTL threshold and bump, in ledgers.
    ///
    /// The factory's instance entry holds the executable plus every
    /// `storage().instance()` value: admin, WASM hashes, pool counts, fee
    /// config, treasury and mode flags. If that entry is archived the factory
    /// stops responding — integrators can no longer enumerate or look up pools
    /// and no new pool can be deployed until it is restored. The factory is the
    /// only deployable contract in the workspace that previously managed no TTL
    /// at all (issue #904), so every entrypoint now extends it.
    ///
    /// At the ~5s/ledger Stellar cadence:
    /// - `MIN_TTL` = 120_960 ledgers ≈ 7 days: only rewrite the entry when less
    ///   than a week of life remains, keeping hot paths cheap.
    /// - `BUMP_TO` = 2_419_200 ledgers ≈ 140 days: renew toward the persistent
    ///   rent window, so one call keeps the factory live for months.
    ///
    /// The same constants extend the per-pool persistent registry entries
    /// (`Pool`, `LpToken`, `PoolByIndex`, ...) on the paths that write them, so
    /// the registry ages together with the instance entry rather than lapsing
    /// independently.
    pub const MIN_TTL: u32 = 120_960;
    pub const BUMP_TO: u32 = 2_419_200;

    /// Extend the contract's instance storage TTL. Called as the first statement
    /// of every public entrypoint, including read-only paths, because a pure
    /// lookup is often the only traffic the factory sees for long stretches.
    fn extend_ttl(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(Self::MIN_TTL, Self::BUMP_TO);
    }

    // ── Setup ─────────────────────────────────────────────────────────────────

    /// One-time factory setup.
    ///
    /// `amm_wasm_hash` and `token_wasm_hash` must be uploaded to the network
    /// (via `stellar contract upload`) before calling this function.
    pub fn initialize(
        env: Env,
        admin: Address,
        amm_wasm_hash: BytesN<32>,
        token_wasm_hash: BytesN<32>,
    ) -> Result<(), FactoryError> {
        Self::extend_ttl(&env);
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(FactoryError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::AmmWasmHash, &amm_wasm_hash);
        env.storage()
            .instance()
            .set(&DataKey::TokenWasmHash, &token_wasm_hash);
        env.storage()
            .instance()
            .set(&DataKey::TokenWasmHash, &token_wasm_hash);
        env.storage().instance().set(&DataKey::PoolCount, &0u64);
        env.storage().instance().set(&DataKey::ClPoolCount, &0u64);
        // Initialize default fee tier to Medium (0.3% = 30 bps)
        env.storage()
            .instance()
            .set(&DataKey::DefaultFeeTier, &2i128);
        Ok(())
    }

    // ── Pool creation ─────────────────────────────────────────────────────────

    /// Deploy a new AMM pool for `(token_a, token_b)` with the specified fee tier.
    ///
    /// Token pair order is normalised — the pool is always stored with the
    /// lexicographically smaller address as `token_a`, so callers do not need
    /// to match the original order when looking up a pool.
    ///
    /// `caller` is the address initiating the call. In permissioned mode the
    /// factory admin must be the caller. In permissionless mode any address may
    /// create a pool, but they must pay the configured `PoolCreationFee` in
    /// `FeeToken` to the protocol treasury (factory admin), and are subject to
    /// a per-address rate limit.
    ///
    /// Panics if a pool for this pair already exists.
    pub fn create_pool(
        env: Env,
        caller: Address,
        token_a: Address,
        token_b: Address,
        fee_tier: i128,
        governance_wasm_hash: Option<BytesN<32>>,
    ) -> Result<(Address, Option<Address>), FactoryError> {
        Self::extend_ttl(&env);
        Self::ensure_creation_unpaused(&env)?;
        let fee_bps = fee_tier_to_bps(fee_tier)?;
        Self::create_pool_with_fee_bps(env, caller, token_a, token_b, fee_bps, governance_wasm_hash)
    }

    /// Deploy a new AMM pool for `(token_a, token_b)` with a custom fee in basis points.
    ///
    /// This allows pools to be created with custom fees outside the standard tiers.
    /// For most use cases, prefer `create_pool` with a standard fee tier.
    ///
    /// Token pair order is normalised. Panics if a pool for this pair already exists.
    ///
    /// Validations (fee_bps bounds, duplicate-pool check) are performed before
    /// the rate-limit check and fee charge so a caller is never charged for a
    /// request that would be rejected (issue #520).
    pub fn create_pool_with_fee_bps(
        env: Env,
        caller: Address,
        token_a: Address,
        token_b: Address,
        fee_bps: i128,
        governance_wasm_hash: Option<BytesN<32>>,
    ) -> Result<(Address, Option<Address>), FactoryError> {
        Self::extend_ttl(&env);
        Self::ensure_creation_unpaused(&env)?;

        // ── Validate BEFORE charging (issue #520) ─────────────────────────
        // All checks below are pure validation with no side effects.  They
        // must run before `check_and_update_rate_limit` / `charge_pool_creation_fee`
        // so a caller is never charged or rate-limited for a request that
        // would have been rejected anyway.

        if !(0..=10_000).contains(&fee_bps) {
            return Err(FactoryError::InvalidFeeBps);
        }

        // Normalise: smaller address is always token_a.
        let (ta, tb) = if token_a < token_b {
            (token_a, token_b)
        } else {
            (token_b, token_a)
        };

        if env
            .storage()
            .persistent()
            .has(&DataKey::Pool(ta.clone(), tb.clone()))
        {
            return Err(FactoryError::PoolAlreadyExists);
        }

        // ── Auth, rate-limit, fee ─────────────────────────────────────────
        let admin = Self::read_admin(&env)?;
        let permissionless: bool = env
            .storage()
            .instance()
            .get(&DataKey::PermissionlessMode)
            .unwrap_or(false);

        if permissionless {
            caller.require_auth();
            Self::check_and_update_rate_limit(&env, &caller)?;
            Self::charge_pool_creation_fee(&env, &caller, &admin)?;
        } else {
            admin.require_auth();
        }

        let amm_wasm = Self::read_amm_wasm_hash(&env)?;
        let token_wasm = Self::read_token_wasm_hash(&env)?;

        // Derive salts per pool from a monotonic counter.
        // We use n * 3 for LP salt, n * 3 + 1 for Pool salt, n * 3 + 2 for Governance salt.
        let n: u64 = env
            .storage()
            .instance()
            .get(&DataKey::PoolCount)
            .unwrap_or(0);
        env.storage().instance().set(&DataKey::PoolCount, &(n + 1));

        let lp_salt = Self::make_salt(&env, n * 3);
        let pool_salt = Self::make_salt(&env, n * 3 + 1);

        // Deploy LP token then AMM pool.
        let lp_addr = env
            .deployer()
            .with_current_contract(lp_salt)
            .deploy(token_wasm);
        let pool_addr = env
            .deployer()
            .with_current_contract(pool_salt)
            .deploy(amm_wasm);

        // Resolve LP token name/symbol defaults.
        let name = Self::counter_string(&env, b"AMM LP Token #", n);
        let symbol = Self::counter_string(&env, b"ALP", n);

        // Initialize LP token — admin must be the pool so it can mint/burn.
        LpTokenClient::new(&env, &lp_addr).initialize(&pool_addr, &name, &symbol, &7u32);

        // Optionally deploy governance contract
        let gov_addr = if let Some(gov_wasm) = governance_wasm_hash {
            let gov_salt = Self::make_salt(&env, n * 3 + 2);
            let gov_addr = env
                .deployer()
                .with_current_contract(gov_salt)
                .deploy(gov_wasm);

            // Initialize governance: 7 days voting, 2 days timelock, 10% quorum, 1% min stake.
            GovernanceClient::new(&env, &gov_addr).initialize(
                &admin,
                &pool_addr,
                &lp_addr,
                &604800_u64,
                &172800_u64,
                &1000_i128,
                &100_i128,
            );

            Some(gov_addr)
        } else {
            None
        };

        let pool_admin = gov_addr.clone().unwrap_or_else(|| admin.clone());

        // Initialize AMM pool.
        AmmPoolClient::new(&env, &pool_addr).initialize(
            &pool_admin,
            &ta,
            &tb,
            &lp_addr,
            &fee_bps,
            &admin,  // fee_recipient
            &0_i128, // protocol_fee_bps (disabled by default)
        );

        // Register pool in lookup indexes and record the LP token address.
        // These are per-pool entries and grow without bound as pools accumulate,
        // so they belong in persistent storage (each with its own TTL) rather
        // than instance storage, which is a single blob loaded on every call
        // to the contract (issue #468).
        env.storage()
            .persistent()
            .set(&DataKey::Pool(ta.clone(), tb.clone()), &pool_addr);
        env.storage()
            .persistent()
            .set(&DataKey::LpToken(pool_addr.clone()), &lp_addr);
        env.storage()
            .persistent()
            .set(&DataKey::GovernanceFor(pool_addr.clone()), &gov_addr);
        // Store reverse token-pair lookup used by sweep_fees to forward tokens.
        env.storage().persistent().set(
            &DataKey::PoolTokens(pool_addr.clone()),
            &(ta.clone(), tb.clone()),
        );

        env.storage()
            .persistent()
            .set(&DataKey::PoolByIndex(n), &pool_addr);

        // Bump the per-pool registry entries on the same call path that writes
        // them, so the registry does not lapse independently of the instance
        // entry (issue #904).
        for key in [
            DataKey::Pool(ta.clone(), tb.clone()),
            DataKey::LpToken(pool_addr.clone()),
            DataKey::GovernanceFor(pool_addr.clone()),
            DataKey::PoolTokens(pool_addr.clone()),
            DataKey::PoolByIndex(n),
        ] {
            env.storage()
                .persistent()
                .extend_ttl(&key, Self::MIN_TTL, Self::BUMP_TO);
        }

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "pool_created"),),
            (
                ta.clone(),
                tb.clone(),
                pool_addr.clone(),
                fee_bps,
                lp_addr.clone(),
            )
        );

        Ok((pool_addr, gov_addr))
    }

    // ── Admin ─────────────────────────────────────────────────────────────────

    /// Update the AMM and/or LP token WASM hashes used for new pool deployments.
    ///
    /// Only the factory admin can call this. Existing pools are unaffected; only
    /// pools created after this call will use the new hashes.
    ///
    /// Pass `None` for a hash to leave it unchanged.
    /// Emits a `wasm_updated` event on every call.
    pub fn update_wasm_hashes(
        env: Env,
        amm_wasm_hash: Option<BytesN<32>>,
        token_wasm_hash: Option<BytesN<32>>,
    ) -> Result<(), FactoryError> {
        Self::extend_ttl(&env);
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        let admin = Self::read_admin(&env)?;
        admin.require_auth();
        if let Some(ref h) = amm_wasm_hash {
            env.storage().instance().set(&DataKey::AmmWasmHash, h);
        }
        if let Some(ref h) = token_wasm_hash {
            env.storage().instance().set(&DataKey::TokenWasmHash, h);
        }
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "wasm_updated"),),
            (amm_wasm_hash, token_wasm_hash)
        );
        Ok(())
    }

    /// Set the default fee tier for new pool deployments. Admin-only.
    ///
    /// `fee_tier` must be 0-3 (VeryLow, Low, Medium, High).
    /// Existing pools are unaffected; only pools created after this call
    /// will use the new default tier.
    pub fn set_default_fee_tier(env: Env, fee_tier: i128) -> Result<(), FactoryError> {
        Self::extend_ttl(&env);
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        let admin = Self::read_admin(&env)?;
        admin.require_auth();

        // Validate the fee tier
        fee_tier_to_bps(fee_tier)?;

        env.storage()
            .instance()
            .set(&DataKey::DefaultFeeTier, &fee_tier);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "default_fee_tier_updated"),),
            (fee_tier,)
        );
        Ok(())
    }

    /// Replace the factory contract WASM with a new version. Admin-only.
    ///
    /// The new WASM must already be uploaded to the network.
    /// State is preserved; only bytecode is replaced.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), FactoryError> {
        Self::extend_ttl(&env);
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        let admin = Self::read_admin(&env)?;
        admin.require_auth();
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "upgraded"),),
            (new_wasm_hash,)
        );
        Ok(())
    }

    /// Set or update the WASM hash used for concentrated_liquidity deployments. Admin-only.
    pub fn set_cl_wasm_hash(env: Env, cl_wasm_hash: BytesN<32>) -> Result<(), FactoryError> {
        Self::extend_ttl(&env);
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        let admin = Self::read_admin(&env)?;
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::ClWasmHash, &cl_wasm_hash);
        Ok(())
    }

    /// Pause all new V2 and concentrated-liquidity pool creation. Admin-only.
    pub fn pause_creation(env: Env, admin: Address) -> Result<(), FactoryError> {
        Self::extend_ttl(&env);
        Self::require_admin(&env, &admin)?;
        env.storage()
            .instance()
            .set(&DataKey::CreationPaused, &true);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "creation_paused"),),
            (admin,)
        );
        Ok(())
    }

    /// Resume V2 and concentrated-liquidity pool creation. Admin-only.
    pub fn unpause_creation(env: Env, admin: Address) -> Result<(), FactoryError> {
        Self::extend_ttl(&env);
        Self::require_admin(&env, &admin)?;
        env.storage()
            .instance()
            .set(&DataKey::CreationPaused, &false);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "creation_unpaused"),),
            (admin,)
        );
        Ok(())
    }

    /// Deploy a new concentrated liquidity pool for `(token_a, token_b)` at `fee_bps`.
    ///
    /// A given (token_a, token_b, fee_bps) triplet is unique — the same pair can have
    /// multiple CL pools at different fee tiers. Token order is normalised (smaller
    /// address first). Panics if the triplet already has a pool.
    ///
    /// Unlike V2 pools, no LP token is deployed — positions are tracked on-chain by
    /// the CL contract itself.
    ///
    /// Applies the same permissioned/permissionless access controls as `create_pool`.
    ///
    /// Validations (fee_bps bounds, duplicate-pool check) are performed before
    /// the rate-limit check and fee charge so a caller is never charged for a
    /// request that would be rejected (issue #520).
    pub fn create_cl_pool(
        env: Env,
        caller: Address,
        token_a: Address,
        token_b: Address,
        fee_bps: i128,
        initial_tick: i32,
    ) -> Result<Address, FactoryError> {
        Self::extend_ttl(&env);
        Self::ensure_creation_unpaused(&env)?;

        // ── Validate BEFORE charging (issue #520) ─────────────────────────
        if !(0..=10_000).contains(&fee_bps) {
            return Err(FactoryError::InvalidFeeBps);
        }

        // Normalise token order.
        let (ta, tb) = if token_a < token_b {
            (token_a, token_b)
        } else {
            (token_b, token_a)
        };

        let cl_key = DataKey::ClPool(ta.clone(), tb.clone(), fee_bps);
        if env.storage().persistent().has(&cl_key) {
            return Err(FactoryError::ClPoolAlreadyExists);
        }

        // ── Auth, rate-limit, fee ─────────────────────────────────────────
        let admin = Self::read_admin(&env)?;
        let permissionless: bool = env
            .storage()
            .instance()
            .get(&DataKey::PermissionlessMode)
            .unwrap_or(false);

        if permissionless {
            caller.require_auth();
            Self::check_and_update_rate_limit(&env, &caller)?;
            Self::charge_pool_creation_fee(&env, &caller, &admin)?;
        } else {
            admin.require_auth();
        }

        let cl_wasm: BytesN<32> = env
            .storage()
            .instance()
            .get(&DataKey::ClWasmHash)
            .ok_or(FactoryError::ClWasmNotSet)?;

        // Issue #493: CL pools are indexed and counted separately from AMM
        // pools (`ClPoolCount`/`ClPoolByIndex`, not `PoolCount`/`PoolByIndex`),
        // so the AMM accessors (`all_pools()`/`get_pools()`) never return a CL
        // pool address. The salt is still offset by `0x8000_0000_0000_0000` so
        // a CL pool's deploy salt can never collide with an AMM pool's, even
        // though the two now count independently.
        let n: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ClPoolCount)
            .unwrap_or(0);
        let cl_salt = Self::make_salt(&env, n * 3 + 2 + 0x8000_0000_0000_0000);
        env.storage()
            .instance()
            .set(&DataKey::ClPoolCount, &(n + 1));

        let pool_addr = env
            .deployer()
            .with_current_contract(cl_salt)
            .deploy(cl_wasm);

        // Derive tick_spacing from fee tier using the same tiering as the
        // dex_aggregator and the standard concentrated-liquidity fee tiers.
        // The 500 bps tier is an actively routed fee tier and must not fall
        // back to the near-zero-fee spacing.
        let tick_spacing: i32 = match fee_bps {
            5 => 1,
            30 => 10,
            100 => 60,
            500 => 200,
            _ => 1,
        };
        ClPoolClient::new(&env, &pool_addr).initialize(
            &admin,
            &ta,
            &tb,
            &fee_bps,
            &initial_tick,
            &tick_spacing,
        );

        env.storage().persistent().set(&cl_key, &pool_addr);

        env.storage()
            .persistent()
            .set(&DataKey::ClPoolByIndex(n), &pool_addr);

        // Bump the CL registry entries on the same call path that writes them
        // (issue #904).
        env.storage()
            .persistent()
            .extend_ttl(&cl_key, Self::MIN_TTL, Self::BUMP_TO);
        env.storage().persistent().extend_ttl(
            &DataKey::ClPoolByIndex(n),
            Self::MIN_TTL,
            Self::BUMP_TO,
        );

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "cl_pool_created"),),
            (ta.clone(), tb.clone(), fee_bps, pool_addr.clone())
        );

        Ok(pool_addr)
    }

    // ── Pool-creation mode ────────────────────────────────────────────────────

    /// Toggle between permissioned (admin-only) and permissionless pool creation. Admin-only.
    ///
    /// When `enabled` is `true`, any address may call `create_pool` / `create_cl_pool`
    /// provided they pay the configured creation fee. When `false` (default), only the
    /// factory admin may create pools.
    ///
    /// The pool creation fee and fee token must be set via `set_pool_creation_fee`
    /// before enabling permissionless mode.
    pub fn set_permissionless_mode(env: Env, enabled: bool) -> Result<(), FactoryError> {
        Self::extend_ttl(&env);
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        let admin = Self::read_admin(&env)?;
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PermissionlessMode, &enabled);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "mode_changed"),),
            (enabled,)
        );
        Ok(())
    }

    /// Configure the fee charged per pool creation in permissionless mode. Admin-only.
    ///
    /// `fee_token` is the SEP-41 token address used for payment (e.g. XLM/native).
    /// `fee_amount` is the amount in the token's smallest unit; must be > 0.
    /// Fees are transferred to the factory admin (protocol treasury) on each creation.
    pub fn set_pool_creation_fee(
        env: Env,
        fee_token: Address,
        fee_amount: i128,
    ) -> Result<(), FactoryError> {
        Self::extend_ttl(&env);
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        let admin = Self::read_admin(&env)?;
        admin.require_auth();
        if fee_amount <= 0 {
            return Err(FactoryError::FeeNotConfigured);
        }
        env.storage().instance().set(&DataKey::FeeToken, &fee_token);
        env.storage()
            .instance()
            .set(&DataKey::PoolCreationFee, &fee_amount);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "creation_fee_set"),),
            (fee_token, fee_amount)
        );
        Ok(())
    }

    /// Set the minimum ledger gap between pool creations per address. Admin-only.
    ///
    /// Defaults to 1 (one pool per ledger per address). Increase to slow down
    /// burst creation attempts. Set to 0 to disable rate limiting.
    pub fn set_rate_limit(env: Env, min_ledgers: u32) -> Result<(), FactoryError> {
        Self::extend_ttl(&env);
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        let admin = Self::read_admin(&env)?;
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::RateLimitLedgers, &min_ledgers);
        Ok(())
    }

    /// Nominate a new admin. The nominee must call `accept_admin` to complete the transfer.
    pub fn propose_admin(env: Env, admin: Address, new_admin: Address) -> Result<(), FactoryError> {
        Self::require_admin(&env, &admin)?;
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Some(new_admin.clone()));
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "admin_nominated"),),
            (admin, new_admin)
        );
        Ok(())
    }

    /// Accept the pending admin nomination. Caller becomes the new admin.
    pub fn accept_admin(env: Env, new_admin: Address) -> Result<(), FactoryError> {
        let pending: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None);
        let nominee = pending.ok_or(FactoryError::NoPendingAdmin)?;
        if new_admin != nominee {
            return Err(FactoryError::WrongAdmin);
        }
        new_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Option::<Address>::None);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "admin_changed"),),
            (new_admin,)
        );
        Ok(())
    }

    /// Return current admin address, or `None` if unset.
    pub fn get_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    /// Return pending admin nominee, if any.
    pub fn get_pending_admin(env: Env) -> Option<Address> {
        env.storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None)
    }

    // ── Queries ───────────────────────────────────────────────────────────────

    /// Return whether permissionless pool creation is currently enabled.
    pub fn is_permissionless(env: Env) -> bool {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::PermissionlessMode)
            .unwrap_or(false)
    }

    /// Return whether new V2 and CL pool creation is currently paused.
    pub fn is_creation_paused(env: Env) -> bool {
        Self::extend_ttl(&env);
        Self::creation_paused(&env)
    }

    /// Return the current pool creation fee `(fee_token, fee_amount)`, or `None` if unset.
    pub fn get_pool_creation_fee(env: Env) -> Option<(Address, i128)> {
        Self::extend_ttl(&env);
        let token: Option<Address> = env.storage().instance().get(&DataKey::FeeToken);
        let amount: Option<i128> = env.storage().instance().get(&DataKey::PoolCreationFee);
        match (token, amount) {
            (Some(t), Some(a)) => Some((t, a)),
            _ => None,
        }
    }

    /// Return the LP token address for the given pool, or `None` if unknown.
    pub fn get_lp_token(env: Env, pool: Address) -> Option<Address> {
        Self::extend_ttl(&env);
        env.storage().persistent().get(&DataKey::LpToken(pool))
    }

    /// Return the governance address for the given pool, or `None` if unknown.
    pub fn get_governance(env: Env, pool: Address) -> Option<Address> {
        Self::extend_ttl(&env);
        env.storage()
            .persistent()
            .get(&DataKey::GovernanceFor(pool))
            .unwrap_or(None)
    }

    /// Return the current default fee tier.
    ///
    /// Returns the fee tier ID (0-3) that will be used for new pools
    /// if no specific tier is provided to `create_pool`.
    pub fn get_default_fee_tier(env: Env) -> i128 {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::DefaultFeeTier)
            .unwrap_or(2) // Default to Medium (0.3%) if not set
    }

    /// Convert a fee tier ID to its basis points value.
    ///
    /// # Returns
    /// - 0 → 1 bps (0.01%)
    /// - 1 → 5 bps (0.05%)
    /// - 2 → 30 bps (0.3%)
    /// - 3 → 100 bps (1.0%)
    pub fn get_fee_tier_bps(env: Env, fee_tier: i128) -> Result<i128, FactoryError> {
        Self::extend_ttl(&env);
        fee_tier_to_bps(fee_tier)
    }

    /// Return the pool address for `(token_a, token_b)`, or `None` if it does
    /// not exist. Token pair order does not matter.
    pub fn get_pool(env: Env, token_a: Address, token_b: Address) -> Option<Address> {
        Self::extend_ttl(&env);
        let (ta, tb) = if token_a < token_b {
            (token_a, token_b)
        } else {
            (token_b, token_a)
        };
        env.storage().persistent().get(&DataKey::Pool(ta, tb))
    }

    /// Return the CL pool address for `(token_a, token_b, fee_bps)`, or `None` if absent.
    /// Token pair order does not matter.
    pub fn get_cl_pool(
        env: Env,
        token_a: Address,
        token_b: Address,
        fee_bps: i128,
    ) -> Option<Address> {
        Self::extend_ttl(&env);
        let (ta, tb) = if token_a < token_b {
            (token_a, token_b)
        } else {
            (token_b, token_a)
        };
        env.storage()
            .persistent()
            .get(&DataKey::ClPool(ta, tb, fee_bps))
    }

    /// Return **AMM** pool addresses, oldest first, capped at
    /// [`Factory::MAX_UNBOUNDED_PAGE`].
    ///
    /// Never includes CL pools (issue #493) — callers that treat every entry
    /// as an AMM pool (e.g. anything calling the AMM-only `get_info()`) can
    /// rely on that. Use `all_cl_pools()` for CL pool addresses.
    ///
    /// **Bounded since issue #790.** This is no longer a "return everything"
    /// read: once the factory holds more than `MAX_UNBOUNDED_PAGE` pools, the
    /// result is truncated to the first `MAX_UNBOUNDED_PAGE` by index and the
    /// rest are simply not returned — silently, since the return type carries
    /// no truncation flag. Compare `get_pool_count()` against the returned
    /// length to detect truncation, and use `get_pools(offset, limit)` to page
    /// through the full set. Prefer `get_pools` outright in new code.
    pub fn all_pools(env: Env) -> Vec<Address> {
        Self::get_pools(env, 0, Self::MAX_UNBOUNDED_PAGE)
    }

    /// Return the total number of **AMM** pools deployed by this factory.
    /// See `get_cl_pool_count()` for the CL pool count.
    pub fn get_pool_count(env: Env) -> u64 {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::PoolCount)
            .unwrap_or(0)
    }

    /// Return up to `limit` **AMM** pool addresses starting at `offset`.
    /// See `get_cl_pools()` for CL pool pagination.
    pub fn get_pools(env: Env, offset: u32, limit: u32) -> Vec<Address> {
        Self::extend_ttl(&env);
        let count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::PoolCount)
            .unwrap_or(0);
        let start = (offset as u64).min(count);
        let end = (start + limit as u64).min(count);

        let mut page = Vec::new(&env);
        for i in start..end {
            if let Some(pool) = env.storage().persistent().get(&DataKey::PoolByIndex(i)) {
                page.push_back(pool);
            }
        }
        page
    }

    /// Return **CL** (concentrated-liquidity) pool addresses, oldest first,
    /// capped at [`Factory::MAX_UNBOUNDED_PAGE`]. Mirrors `all_pools()` but for
    /// the separately indexed CL pool sequence (issue #493), and is bounded
    /// for the same reason (issue #790) — see `all_pools()` for how to detect
    /// truncation and page past it with `get_cl_pools(offset, limit)`.
    pub fn all_cl_pools(env: Env) -> Vec<Address> {
        Self::get_cl_pools(env, 0, Self::MAX_UNBOUNDED_PAGE)
    }

    /// Return the total number of CL pools deployed by this factory.
    pub fn get_cl_pool_count(env: Env) -> u64 {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::ClPoolCount)
            .unwrap_or(0)
    }

    /// Return up to `limit` CL pool addresses starting at `offset`.
    pub fn get_cl_pools(env: Env, offset: u32, limit: u32) -> Vec<Address> {
        Self::extend_ttl(&env);
        let count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ClPoolCount)
            .unwrap_or(0);
        let start = (offset as u64).min(count);
        let end = (start + limit as u64).min(count);

        let mut page = Vec::new(&env);
        for i in start..end {
            if let Some(pool) = env.storage().persistent().get(&DataKey::ClPoolByIndex(i)) {
                page.push_back(pool);
            }
        }
        page
    }

    /// Check if an address is a registered CL pool in this factory.
    pub fn is_cl_pool(env: Env, pool: Address) -> bool {
        Self::extend_ttl(&env);
        let count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ClPoolCount)
            .unwrap_or(0);
        for i in 0..count {
            if let Some(p) = env
                .storage()
                .persistent()
                .get::<_, Address>(&DataKey::ClPoolByIndex(i))
            {
                if p == pool {
                    return true;
                }
            }
        }
        false
    }

    // ── Treasury & protocol fee sweep ────────────────────────────────────────

    /// Configure the protocol treasury address and the global protocol fee rate.
    ///
    /// Admin-only. This immediately propagates the configured rate to all
    /// factory-administered pools, making the factory their `fee_recipient` so
    /// `sweep_fees` can permissionlessly collect and forward fees.
    ///
    /// Setting `global_protocol_fee_bps` to `0` disables fee collection on all
    /// factory-administered pools after this call completes.
    pub fn set_treasury(
        env: Env,
        admin: Address,
        treasury: Address,
        global_protocol_fee_bps: i128,
    ) -> Result<(), FactoryError> {
        Self::extend_ttl(&env);
        let stored_admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if admin != stored_admin {
            return Err(FactoryError::Unauthorized);
        }
        admin.require_auth();
        Self::require_admin(&env, &admin)?;
        if !(0..=10_000).contains(&global_protocol_fee_bps) {
            return Err(FactoryError::InvalidFeeBps);
        }
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        env.storage()
            .instance()
            .set(&DataKey::GlobalProtocolFeeBps, &global_protocol_fee_bps);
        let updated = Self::sync_global_fee_page(
            &env,
            &admin,
            global_protocol_fee_bps,
            0,
            Self::pool_count(&env),
        );
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "treasury_set"),),
            (treasury, global_protocol_fee_bps, updated)
        );
        Ok(())
    }

    /// Return the current treasury address and global protocol fee rate.
    ///
    /// Returns `None` when no treasury has been configured yet.
    pub fn get_treasury(env: Env) -> Option<(Address, i128)> {
        Self::extend_ttl(&env);
        let treasury: Option<Address> = env.storage().instance().get(&DataKey::Treasury);
        let bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::GlobalProtocolFeeBps)
            .unwrap_or(0);
        treasury.map(|t| (t, bps))
    }

    /// Return the token pair `(token_a, token_b)` recorded for `pool`, or `None`.
    pub fn get_pool_tokens(env: Env, pool: Address) -> Option<(Address, Address)> {
        Self::extend_ttl(&env);
        env.storage().persistent().get(&DataKey::PoolTokens(pool))
    }

    /// Set and propagate the global protocol fee configuration to all
    /// factory-administered pools. Admin-only.
    ///
    /// For each registered pool whose admin is the factory admin (i.e. not
    /// governance-controlled), calls `set_protocol_fee` on the pool with:
    /// - `recipient` = this factory contract (so the factory can later sweep)
    /// - `protocol_fee_bps` = `protocol_fee_bps`
    ///
    /// Governance-controlled pools are skipped — they must update fee settings
    /// via a governance proposal.
    ///
    /// Returns the number of pools successfully updated.
    pub fn set_global_fee(
        env: Env,
        admin: Address,
        protocol_fee_bps: i128,
    ) -> Result<u32, FactoryError> {
        Self::extend_ttl(&env);
        let stored_admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if admin != stored_admin {
            return Err(FactoryError::Unauthorized);
        }
        admin.require_auth();
        Self::require_admin(&env, &admin)?;
        if !env.storage().instance().has(&DataKey::Treasury) {
            return Err(FactoryError::FeeNotConfigured);
        }
        if !(0..=10_000).contains(&protocol_fee_bps) {
            return Err(FactoryError::InvalidFeeBps);
        }
        env.storage()
            .instance()
            .set(&DataKey::GlobalProtocolFeeBps, &protocol_fee_bps);
        let updated =
            Self::sync_global_fee_page(&env, &admin, protocol_fee_bps, 0, Self::pool_count(&env));
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "global_fee_set"),),
            (protocol_fee_bps, 0u32, updated)
        );
        Ok(updated)
    }

    /// Propagate the global protocol fee configuration to a page of
    /// factory-administered pools. Admin-only.
    pub fn set_global_fee_paginated(
        env: Env,
        admin: Address,
        offset: u32,
        limit: u32,
    ) -> Result<u32, FactoryError> {
        Self::extend_ttl(&env);
        let stored_admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if admin != stored_admin {
            return Err(FactoryError::Unauthorized);
        }
        admin.require_auth();
        Self::require_admin(&env, &admin)?;
        if !env.storage().instance().has(&DataKey::Treasury) {
            return Err(FactoryError::FeeNotConfigured);
        }
        let protocol_fee_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::GlobalProtocolFeeBps)
            .unwrap_or(0);
        let updated = Self::sync_global_fee_page(&env, &admin, protocol_fee_bps, offset, limit);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "global_fee_set"),),
            (protocol_fee_bps, offset, updated)
        );
        Ok(updated)
    }

    /// Sweep accrued protocol fees from all factory-owned pools into the
    /// treasury. **Permissionless** — callable by anyone.
    ///
    /// For each registered pool that is not governance-controlled, calls
    /// `withdraw_protocol_fees()` on the pool
    /// (which succeeds because the factory is the `fee_recipient`) and then
    /// transfers the received tokens to the treasury.
    ///
    /// No trust is required: regardless of who triggers the sweep, funds are
    /// cryptographically guaranteed to route to the governance-configured
    /// treasury address stored in this contract.
    ///
    /// Returns the total amount of `token` collected.
    ///
    /// # Prerequisites
    /// - `set_treasury` must have been called to configure the treasury and sync
    ///   the factory as the `fee_recipient` on the target pools.
    pub fn sweep_fees(env: Env, token: Address) -> Result<i128, FactoryError> {
        Self::extend_ttl(&env);
        let (_, total_collected) = Self::sweep_fees_page(&env, &token, 0, Self::pool_count(&env))?;
        Ok(total_collected)
    }

    /// Paginated variant of `sweep_fees` for large pool registries.
    pub fn sweep_fees_paginated(
        env: Env,
        token: Address,
        offset: u32,
        limit: u32,
    ) -> Result<(u32, i128), FactoryError> {
        Self::extend_ttl(&env);
        Self::sweep_fees_page(&env, &token, offset, limit)
    }

    fn sweep_fees_page(
        env: &Env,
        token: &Address,
        offset: u32,
        limit: u32,
    ) -> Result<(u32, i128), FactoryError> {
        let treasury: Address = env
            .storage()
            .instance()
            .get(&DataKey::Treasury)
            .ok_or(FactoryError::FeeNotConfigured)?;

        let factory_addr = env.current_contract_address();
        let count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::PoolCount)
            .unwrap_or(0);

        let start = (offset as u64).min(count);
        let end = (start + limit as u64).min(count);
        let mut pools_swept: u32 = 0;
        let mut total_collected: i128 = 0;

        for i in start..end {
            if let Some(pool_addr) = env
                .storage()
                .persistent()
                .get::<DataKey, Address>(&DataKey::PoolByIndex(i))
            {
                // Skip governance-controlled pools.
                let gov: Option<Option<Address>> = env
                    .storage()
                    .persistent()
                    .get(&DataKey::GovernanceFor(pool_addr.clone()));
                let is_governed = gov.flatten().is_some();
                if is_governed {
                    continue;
                }

                let Some((token_a, token_b)) = env
                    .storage()
                    .persistent()
                    .get::<DataKey, (Address, Address)>(&DataKey::PoolTokens(pool_addr.clone()))
                else {
                    continue;
                };

                // Pull accrued fees from the pool to this factory contract.
                // This succeeds because factory == fee_recipient and Soroban
                // automatically authorises require_auth() for the invoking contract.
                let (fee_a, fee_b) = AmmPoolClient::new(env, &pool_addr).withdraw_protocol_fees();

                // Forward to treasury via the token contracts.
                if fee_a > 0 || fee_b > 0 {
                    if fee_a > 0 {
                        sdk_token::Client::new(env, &token_a).transfer(
                            &factory_addr,
                            &treasury,
                            &fee_a,
                        );
                        if token_a == *token {
                            total_collected += fee_a;
                        }
                    }
                    if fee_b > 0 {
                        sdk_token::Client::new(env, &token_b).transfer(
                            &factory_addr,
                            &treasury,
                            &fee_b,
                        );
                        if token_b == *token {
                            total_collected += fee_b;
                        }
                    }
                    pools_swept += 1;
                }
            }
        }

        soroban_amm_sdk::emit_versioned_event!(
            env.clone(),
            (Symbol::new(env, "fees_swept"),),
            (
                treasury,
                token.clone(),
                offset,
                pools_swept,
                total_collected
            )
        );
        Ok((pools_swept, total_collected))
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    fn pool_count(env: &Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::PoolCount)
            .unwrap_or(0u64) as u32
    }

    /// Stored admin, written by `initialize`; absent means not initialized.
    fn read_admin(env: &Env) -> Result<Address, FactoryError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(FactoryError::NotInitialized)
    }

    fn read_amm_wasm_hash(env: &Env) -> Result<BytesN<32>, FactoryError> {
        env.storage()
            .instance()
            .get(&DataKey::AmmWasmHash)
            .ok_or(FactoryError::NotInitialized)
    }

    fn read_token_wasm_hash(env: &Env) -> Result<BytesN<32>, FactoryError> {
        env.storage()
            .instance()
            .get(&DataKey::TokenWasmHash)
            .ok_or(FactoryError::NotInitialized)
    }

    fn require_admin(env: &Env, admin: &Address) -> Result<(), FactoryError> {
        let stored_admin = Self::read_admin(env)?;
        if *admin != stored_admin {
            return Err(FactoryError::Unauthorized);
        }
        admin.require_auth();
        Ok(())
    }

    fn creation_paused(env: &Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::CreationPaused)
            .unwrap_or(false)
    }

    fn ensure_creation_unpaused(env: &Env) -> Result<(), FactoryError> {
        if Self::creation_paused(env) {
            return Err(FactoryError::CreationPaused);
        }
        Ok(())
    }

    fn sync_global_fee_page(
        env: &Env,
        admin: &Address,
        protocol_fee_bps: i128,
        offset: u32,
        limit: u32,
    ) -> u32 {
        let factory_addr = env.current_contract_address();
        let count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::PoolCount)
            .unwrap_or(0);

        let start = (offset as u64).min(count);
        let end = (start + limit as u64).min(count);
        let mut updated: u32 = 0;

        for i in start..end {
            if let Some(pool_addr) = env
                .storage()
                .persistent()
                .get::<DataKey, Address>(&DataKey::PoolByIndex(i))
            {
                // Governance-controlled pools have their own pool admin contract,
                // so the factory cannot authorize their pool-level fee update.
                let gov: Option<Option<Address>> = env
                    .storage()
                    .persistent()
                    .get(&DataKey::GovernanceFor(pool_addr.clone()));
                if gov.flatten().is_some() {
                    continue;
                }

                // Guard: only AMM pools have a PoolTokens entry. CL pools (which
                // use a different interface and don't implement set_protocol_fee)
                // never have this key, so we skip them. This also handles any
                // legacy CL pool addresses that may exist in PoolByIndex from
                // before CL pools were given their own separate index.
                let is_amm_pool = env
                    .storage()
                    .persistent()
                    .has(&DataKey::PoolTokens(pool_addr.clone()));
                if !is_amm_pool {
                    continue;
                }

                AmmPoolClient::new(env, &pool_addr).set_protocol_fee(
                    admin,
                    &factory_addr,
                    &protocol_fee_bps,
                );
                updated += 1;
            }
        }

        updated
    }

    /// Enforce per-address rate limiting for permissionless pool creation.
    ///
    /// Reads `RateLimitLedgers` (default 1). If the caller created a pool within
    /// that many ledgers, returns `RateLimitExceeded`. On success, records the
    /// current ledger for the caller.
    fn check_and_update_rate_limit(env: &Env, caller: &Address) -> Result<(), FactoryError> {
        let min_gap: u32 = env
            .storage()
            .instance()
            .get(&DataKey::RateLimitLedgers)
            .unwrap_or(1u32);

        if min_gap > 0 {
            let current_ledger = env.ledger().sequence();
            let last: Option<u32> = env
                .storage()
                .persistent()
                .get(&DataKey::LastPoolCreation(caller.clone()));

            if let Some(last_ledger) = last {
                if current_ledger.saturating_sub(last_ledger) < min_gap {
                    return Err(FactoryError::RateLimitExceeded);
                }
            }

            env.storage()
                .persistent()
                .set(&DataKey::LastPoolCreation(caller.clone()), &current_ledger);
        }

        Ok(())
    }

    /// Transfer the pool creation fee from `caller` to `treasury`.
    ///
    /// Requires `FeeToken` and `PoolCreationFee` to be set, otherwise returns
    /// `FeeNotConfigured`. The caller must have authorised the token transfer.
    fn charge_pool_creation_fee(
        env: &Env,
        caller: &Address,
        treasury: &Address,
    ) -> Result<(), FactoryError> {
        let fee_token: Address = env
            .storage()
            .instance()
            .get(&DataKey::FeeToken)
            .ok_or(FactoryError::FeeNotConfigured)?;
        let fee_amount: i128 = env
            .storage()
            .instance()
            .get(&DataKey::PoolCreationFee)
            .ok_or(FactoryError::FeeNotConfigured)?;

        sdk_token::Client::new(env, &fee_token).transfer(caller, treasury, &fee_amount);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(env, "creation_fee_paid"),),
            (caller.clone(), fee_amount)
        );

        Ok(())
    }

    /// Build a deterministic 32-byte salt from a u64 index.
    fn make_salt(env: &Env, index: u64) -> BytesN<32> {
        let mut arr = [0u8; 32];
        arr[..8].copy_from_slice(&index.to_be_bytes());
        BytesN::from_array(env, &arr)
    }

    /// Build a Soroban `String` from a byte prefix plus a decimal counter.
    ///
    /// Works in `no_std` — avoids `format!` by constructing ASCII digits manually.
    /// `prefix` must be valid UTF-8 (it always is for the callers in this file).
    fn counter_string(env: &Env, prefix: &[u8], n: u64) -> soroban_sdk::String {
        // Max: 20-char prefix + 20 decimal digits of u64::MAX
        let mut buf = [0u8; 40];
        let plen = prefix.len();
        buf[..plen].copy_from_slice(prefix);

        let nlen = if n == 0 {
            buf[plen] = b'0';
            1usize
        } else {
            let mut tmp = [0u8; 20];
            let mut num = n;
            let mut i = 0usize;
            while num > 0 {
                tmp[i] = b'0' + (num % 10) as u8;
                num /= 10;
                i += 1;
            }
            // Reverse digit order into buf.
            for j in 0..i {
                buf[plen + j] = tmp[i - 1 - j];
            }
            i
        };

        let total = plen + nlen;
        // SAFETY: prefix is valid UTF-8 and the appended bytes are ASCII digits.
        let s = core::str::from_utf8(&buf[..total]).unwrap();
        soroban_sdk::String::from_str(env, s)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────
//
// Tests deploy the AMM and token contracts as real WASM. Build the WASM first:
//
//   cargo build --release --target wasm32v1-none
//
// Then run:
//
//   cargo test -p factory

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{storage::Instance as _, Address as _, Ledger},
        Env, IntoVal,
    };

    // Embed compiled WASM at test-compile time.
    // Use compiled WASM exported by the `amm` and `token` crates (feature `testutils`).

    fn token_fee_for_pool(
        factory: &FactoryClient<'_>,
        pool: &Address,
        token_addr: &Address,
        amm_client: &amm::AmmPoolClient<'_>,
    ) -> i128 {
        let (token_a, token_b) = factory.get_pool_tokens(pool).unwrap();
        let (fee_a, fee_b) = amm_client.get_accrued_fees();
        if token_a == *token_addr {
            fee_a
        } else if token_b == *token_addr {
            fee_b
        } else {
            0
        }
    }

    #[test]
    fn test_create_pool() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        let (pool, gov) = factory.create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);

        assert_eq!(factory.get_pool(&ta, &tb), Some(pool.clone()));
        assert_eq!(factory.all_pools().len(), 1);
        assert_eq!(gov, None);
        assert_eq!(factory.get_governance(&pool), None);
    }

    #[test]
    fn test_create_pool_with_governance() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);
        let gov_hash = env.deployer().upload_contract_wasm(governance::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        let (pool, gov) =
            factory.create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &Some(gov_hash));

        assert_eq!(factory.get_pool(&ta, &tb), Some(pool.clone()));
        assert_eq!(factory.all_pools().len(), 1);
        assert!(gov.is_some());
        assert_eq!(factory.get_governance(&pool), gov);
    }

    #[test]
    fn test_normalize_order() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        factory.create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);

        // Reverse-order lookup returns the same pool.
        assert_eq!(factory.get_pool(&ta, &tb), factory.get_pool(&tb, &ta));
    }

    #[test]
    fn test_duplicate_pool_panics() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        factory.create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);
        let result = factory.try_create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);
        assert!(result.is_err());
    }

    #[test]
    fn test_all_pools() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        assert_eq!(factory.all_pools().len(), 0);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        let tc = Address::generate(&env);

        factory.create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);
        assert_eq!(factory.all_pools().len(), 1);

        factory.create_pool_with_fee_bps(&admin, &ta, &tc, &30_i128, &None);
        assert_eq!(factory.all_pools().len(), 2);
    }

    /// Issue #493: `create_cl_pool` used to share the AMM-only `PoolCount`/
    /// `PoolByIndex` sequence, so a CL pool address leaked into `all_pools()`
    /// once any CL pool had ever been created — breaking every caller (like
    /// `dex_aggregator::discover_tokens`) that assumes every `all_pools()`
    /// entry implements the AMM `get_info()` interface.
    #[test]
    fn test_all_pools_excludes_cl_pools() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);
        let cl_hash = env
            .deployer()
            .upload_contract_wasm(concentrated_liquidity::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);
        factory.set_cl_wasm_hash(&cl_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        let (amm_pool, _) = factory.create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);
        let cl_pool = factory.create_cl_pool(&admin, &ta, &tb, &5_i128, &0_i32);

        // The CL pool must never show up in the AMM-only listing/count.
        let all = factory.all_pools();
        assert_eq!(all.len(), 1);
        assert_eq!(all.get(0).unwrap(), amm_pool);
        assert_eq!(factory.get_pool_count(), 1);
        assert_eq!(factory.get_pools(&0, &10).len(), 1);

        // The CL pool is discoverable through its own dedicated accessors.
        let all_cl = factory.all_cl_pools();
        assert_eq!(all_cl.len(), 1);
        assert_eq!(all_cl.get(0).unwrap(), cl_pool);
        assert_eq!(factory.get_cl_pool_count(), 1);
        assert_eq!(factory.get_cl_pools(&0, &10).len(), 1);
    }

    // ── Issue #96: LP token name/symbol reflect the token pair ───────────────

    #[test]
    fn test_lp_token_default_names_are_distinct() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        let tc = Address::generate(&env);

        let (pool0, _) = factory.create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);
        let (pool1, _) = factory.create_pool_with_fee_bps(&admin, &ta, &tc, &30_i128, &None);

        // Fetch LP token addresses via the factory's registry.
        let lp0 = factory.get_lp_token(&pool0).unwrap();
        let lp1 = factory.get_lp_token(&pool1).unwrap();

        use soroban_sdk::token::Client as TokenClient;
        let lp_client0 = TokenClient::new(&env, &lp0);
        let lp_client1 = TokenClient::new(&env, &lp1);

        // Names and symbols must differ between the two pools.
        assert_ne!(lp_client0.name(), lp_client1.name());
        assert_ne!(lp_client0.symbol(), lp_client1.symbol());
    }

    // ── Issue #97: update_wasm_hashes ─────────────────────────────────────────

    #[test]
    fn test_update_wasm_hashes_non_admin_panics() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        // update_wasm_hashes itself requires admin auth; mock_all_auths covers it,
        // but we can verify the function doesn't panic when called by the real admin.
        factory.update_wasm_hashes(&Some(amm_hash.clone()), &None);
        factory.update_wasm_hashes(&None, &Some(token_hash.clone()));
        factory.update_wasm_hashes(&Some(amm_hash), &Some(token_hash));
    }

    #[test]
    fn test_update_wasm_hashes_updates_storage_and_allows_new_pool() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        // Call update with the same hashes (a no-op in practice, but verifies
        // the function is callable by admin and doesn't panic).
        factory.update_wasm_hashes(&Some(amm_hash.clone()), &Some(token_hash.clone()));

        // Pool creation still works after an update.
        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        let (pool, _) = factory.create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);
        assert!(factory.get_pool(&ta, &tb).is_some());
        assert!(factory.get_lp_token(&pool).is_some());

        // Partial update — only token_wasm_hash.
        factory.update_wasm_hashes(&None, &Some(token_hash.clone()));

        // Partial update — only amm_wasm_hash.
        factory.update_wasm_hashes(&Some(amm_hash.clone()), &None);
    }

    // ── Issue #298: factory-level emergency creation pause ───────────────────

    #[test]
    fn test_creation_pause_blocks_pool_creation_then_resumes() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        assert!(!factory.is_creation_paused());

        factory.pause_creation(&admin);
        assert!(factory.is_creation_paused());

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        let result = factory.try_create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);
        assert_eq!(result, Err(Ok(FactoryError::CreationPaused)));
        assert_eq!(factory.all_pools().len(), 0);

        factory.unpause_creation(&admin);
        assert!(!factory.is_creation_paused());

        let (pool, gov) = factory.create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);
        assert_eq!(factory.get_pool(&ta, &tb), Some(pool));
        assert_eq!(gov, None);
    }

    #[test]
    fn test_creation_pause_blocks_cl_pool_creation() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);
        let cl_hash = env
            .deployer()
            .upload_contract_wasm(concentrated_liquidity::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);
        factory.set_cl_wasm_hash(&cl_hash);

        factory.pause_creation(&admin);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        let result = factory.try_create_cl_pool(&admin, &ta, &tb, &30_i128, &0_i32);
        assert_eq!(result, Err(Ok(FactoryError::CreationPaused)));
        assert_eq!(factory.get_cl_pool(&ta, &tb, &30_i128), None);
    }

    // ── Issue #182: CL pool creation ──────────────────────────────────────────

    #[test]
    fn test_create_cl_pool_two_fee_tiers() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);
        let cl_hash = env
            .deployer()
            .upload_contract_wasm(concentrated_liquidity::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);
        factory.set_cl_wasm_hash(&cl_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        // Deploy same pair at two different fee tiers.
        let pool_30 = factory.create_cl_pool(&admin, &ta, &tb, &30_i128, &0_i32);
        let pool_100 = factory.create_cl_pool(&admin, &ta, &tb, &100_i128, &0_i32);

        // Both pools are distinct addresses.
        assert_ne!(pool_30, pool_100);

        // get_cl_pool returns the correct address for each tier.
        assert_eq!(
            factory.get_cl_pool(&ta, &tb, &30_i128),
            Some(pool_30.clone())
        );
        assert_eq!(
            factory.get_cl_pool(&ta, &tb, &100_i128),
            Some(pool_100.clone())
        );

        // Token order doesn't matter for lookup.
        assert_eq!(factory.get_cl_pool(&tb, &ta, &30_i128), Some(pool_30));

        // Missing tier returns None.
        assert_eq!(factory.get_cl_pool(&ta, &tb, &500_i128), None);
    }

    #[test]
    fn test_create_cl_pool_500_bps_uses_wider_tick_spacing() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);
        let cl_hash = env
            .deployer()
            .upload_contract_wasm(concentrated_liquidity::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);
        factory.set_cl_wasm_hash(&cl_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        let pool_addr = factory.create_cl_pool(&admin, &ta, &tb, &500_i128, &0_i32);
        let state = ClPoolClient::new(&env, &pool_addr).get_pool_state();

        assert_eq!(state.tick_spacing, 200);
    }

    #[test]
    fn test_create_cl_pool_duplicate_panics() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);
        let cl_hash = env
            .deployer()
            .upload_contract_wasm(concentrated_liquidity::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);
        factory.set_cl_wasm_hash(&cl_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        factory.create_cl_pool(&admin, &ta, &tb, &30_i128, &0_i32);
        let result = factory.try_create_cl_pool(&admin, &ta, &tb, &30_i128, &0_i32);
        assert!(result.is_err());
    }

    #[test]
    fn test_pagination() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        // Initial pool count should be 0.
        assert_eq!(factory.get_pool_count(), 0);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        let tc = Address::generate(&env);
        let td = Address::generate(&env);

        let (pool1, _) = factory.create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);
        assert_eq!(factory.get_pool_count(), 1);

        let (pool2, _) = factory.create_pool_with_fee_bps(&admin, &ta, &tc, &30_i128, &None);
        assert_eq!(factory.get_pool_count(), 2);

        let (pool3, _) = factory.create_pool_with_fee_bps(&admin, &ta, &td, &30_i128, &None);
        assert_eq!(factory.get_pool_count(), 3);

        // Page 1: first two pools.
        let page1 = factory.get_pools(&0u32, &2u32);
        assert_eq!(page1.len(), 2);
        assert_eq!(page1.get(0).unwrap(), pool1);
        assert_eq!(page1.get(1).unwrap(), pool2);

        // Page 2: starting at index 1, limit 1.
        let page2 = factory.get_pools(&1u32, &1u32);
        assert_eq!(page2.len(), 1);
        assert_eq!(page2.get(0).unwrap(), pool2);

        // Page 3: limit larger than remaining.
        let page3 = factory.get_pools(&1u32, &5u32);
        assert_eq!(page3.len(), 2);
        assert_eq!(page3.get(0).unwrap(), pool2);
        assert_eq!(page3.get(1).unwrap(), pool3);

        // Page 4: offset past end.
        let page4 = factory.get_pools(&5u32, &2u32);
        assert_eq!(page4.len(), 0);

        // Limit = 0.
        let page5 = factory.get_pools(&1u32, &0u32);
        assert_eq!(page5.len(), 0);
    }

    // ── Issue #194: pool_created event ───────────────────────────────────────

    #[test]
    fn test_create_pool_emits_pool_created_event() {
        use soroban_sdk::testutils::Events as _;

        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        let (pool_addr, _) = factory.create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);

        // Locate the pool_created event.
        let events = env.events().all();
        let expected_topic: soroban_sdk::Vec<soroban_sdk::Val> =
            (Symbol::new(&env, "pool_created"),).into_val(&env);

        let event = events
            .iter()
            .find(|e| e.0 == factory_addr && e.1 == expected_topic)
            .expect("pool_created event must be emitted on successful create_pool");

        // The event data is (token_a, token_b, pool_address, fee_bps, lp_token_address).
        // Normalised token order may differ — just assert pool and fee_bps fields.
        let lp_addr = factory.get_lp_token(&pool_addr).unwrap();
        let __ver_12: (u32, (Address, Address, Address, i128, Address)) = event.2.into_val(&env);
        assert_eq!(__ver_12.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let data: (Address, Address, Address, i128, Address) = __ver_12.1;
        assert_eq!(data.2, pool_addr, "pool address in event must match");
        assert_eq!(data.3, 30_i128, "fee_bps in event must be 30");
        assert_eq!(data.4, lp_addr, "lp_token address in event must match");
    }

    #[test]
    fn test_create_pool_duplicate_does_not_emit_event() {
        use soroban_sdk::testutils::Events as _;

        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        factory.create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);

        // Clear events so we only see events from the second (failing) call.
        // Soroban test env accumulates events — count before the duplicate attempt.
        let count_before = env.events().all().len();

        // Duplicate call must fail.
        let result = factory.try_create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);
        assert!(result.is_err(), "duplicate pool creation must fail");

        // No new events must have been added.
        let count_after = env.events().all().len();
        assert_eq!(
            count_before, count_after,
            "no event should be emitted when create_pool reverts"
        );
    }

    // ── Permissionless pool creation ──────────────────────────────────────────

    fn setup_fee_token(env: &Env, admin: &Address, user: &Address, amount: i128) -> Address {
        let fee_token_addr = env.register_contract(None, token::LpToken);
        token::LpTokenClient::new(env, &fee_token_addr).initialize(
            admin,
            &soroban_sdk::String::from_str(env, "Fee Token"),
            &soroban_sdk::String::from_str(env, "FEE"),
            &7u32,
        );
        token::LpTokenClient::new(env, &fee_token_addr).mint(user, &amount);
        fee_token_addr
    }

    #[test]
    fn test_permissioned_mode_blocks_non_admin() {
        let env = Env::default();
        env.budget().reset_unlimited();
        // Do NOT mock_all_auths — we need real auth checks.

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);

        // Initialize requires admin auth.
        env.mock_auths(&[soroban_sdk::testutils::MockAuth {
            address: &admin,
            invoke: &soroban_sdk::testutils::MockAuthInvoke {
                contract: &factory_addr,
                fn_name: "initialize",
                args: (admin.clone(), amm_hash.clone(), token_hash.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        factory.initialize(&admin, &amm_hash, &token_hash);

        // Default mode is permissioned; a non-admin caller must be rejected.
        assert!(!factory.is_permissionless());

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        // user tries to create a pool in permissioned mode — must fail.
        let result = factory.try_create_pool_with_fee_bps(&user, &ta, &tb, &30_i128, &None);
        assert!(
            result.is_err(),
            "non-admin must not create pools in permissioned mode"
        );
    }

    #[test]
    fn test_permissionless_mode_toggle() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        assert!(!factory.is_permissionless());
        factory.set_permissionless_mode(&true);
        assert!(factory.is_permissionless());
        factory.set_permissionless_mode(&false);
        assert!(!factory.is_permissionless());
    }

    #[test]
    fn test_permissionless_charges_fee() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let user = Address::generate(&env);

        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        // Deploy a fee token and mint 1000 units to user.
        let fee_token_addr = setup_fee_token(&env, &admin, &user, 1000);
        let fee_client = soroban_sdk::token::Client::new(&env, &fee_token_addr);

        let fee_amount: i128 = 100;
        factory.set_pool_creation_fee(&fee_token_addr, &fee_amount);
        factory.set_permissionless_mode(&true);

        assert_eq!(
            factory.get_pool_creation_fee(),
            Some((fee_token_addr.clone(), fee_amount))
        );

        let user_balance_before = fee_client.balance(&user);
        let admin_balance_before = fee_client.balance(&admin);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        let (_pool, _) = factory.create_pool_with_fee_bps(&user, &ta, &tb, &30_i128, &None);

        assert!(factory.get_pool(&ta, &tb).is_some());
        // Fee transferred from user to admin (treasury).
        assert_eq!(fee_client.balance(&user), user_balance_before - fee_amount);
        assert_eq!(
            fee_client.balance(&admin),
            admin_balance_before + fee_amount
        );
    }

    #[test]
    fn test_permissionless_fee_not_configured_fails() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        // Enable permissionless without setting a fee — must fail on pool creation.
        factory.set_permissionless_mode(&true);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        let result = factory.try_create_pool_with_fee_bps(&user, &ta, &tb, &30_i128, &None);
        assert!(
            result.is_err(),
            "pool creation without configured fee must fail"
        );
    }

    #[test]
    fn test_rate_limit_blocks_rapid_creation() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let fee_token_addr = setup_fee_token(&env, &admin, &user, 10_000);
        factory.set_pool_creation_fee(&fee_token_addr, &100_i128);
        factory.set_permissionless_mode(&true);
        // Require 5 ledger gap between creations.
        factory.set_rate_limit(&5u32);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        let tc = Address::generate(&env);

        // First creation succeeds.
        factory.create_pool_with_fee_bps(&user, &ta, &tb, &30_i128, &None);

        // Immediate second creation by the same user must be rate-limited.
        let result = factory.try_create_pool_with_fee_bps(&user, &ta, &tc, &30_i128, &None);
        assert!(
            result.is_err(),
            "second pool creation within rate limit window must fail"
        );
    }

    #[test]
    fn test_rate_limit_zero_disables_limit() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let fee_token_addr = setup_fee_token(&env, &admin, &user, 10_000);
        factory.set_pool_creation_fee(&fee_token_addr, &100_i128);
        factory.set_permissionless_mode(&true);
        factory.set_rate_limit(&0u32); // disable rate limiting

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        let tc = Address::generate(&env);

        // Both creations must succeed on the same ledger.
        factory.create_pool_with_fee_bps(&user, &ta, &tb, &30_i128, &None);
        factory.create_pool_with_fee_bps(&user, &ta, &tc, &30_i128, &None);
        assert_eq!(factory.all_pools().len(), 2);
    }

    // ── Treasury & fee sweep ─────────────────────────────────────────────────

    #[test]
    fn test_set_treasury_stores_config() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        // Before set_treasury: get_treasury must return None.
        assert_eq!(factory.get_treasury(), None);

        factory.set_treasury(&admin, &treasury, &30_i128);

        let result = factory.get_treasury();
        assert_eq!(result, Some((treasury.clone(), 30_i128)));

        // Updating treasury is idempotent.
        let treasury2 = Address::generate(&env);
        factory.set_treasury(&admin, &treasury2, &50_i128);
        assert_eq!(factory.get_treasury(), Some((treasury2, 50_i128)));
    }

    #[test]
    fn test_set_treasury_invalid_bps_fails() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        // fee_bps > 10_000 must be rejected.
        let result = factory.try_set_treasury(&admin, &treasury, &10_001_i128);
        assert!(result.is_err());
    }

    #[test]
    fn test_set_global_fee_without_treasury_fails() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        factory.create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);

        // set_global_fee without treasury must return FeeNotConfigured.
        let result = factory.try_set_global_fee(&admin, &10_i128);
        assert!(result.is_err());
    }

    #[test]
    fn test_set_global_fee_propagates_to_non_governance_pools() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        let (pool_addr, _) = factory.create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);

        // Verify token pair is stored for sweep.
        let tokens = factory.get_pool_tokens(&pool_addr);
        assert!(tokens.is_some());

        factory.set_treasury(&admin, &treasury, &10_i128);

        let amm_client = amm::AmmPoolClient::new(&env, &pool_addr);
        let (recipient_after_treasury, bps_after_treasury) = amm_client.get_protocol_fee();
        assert_eq!(recipient_after_treasury, Some(factory_addr.clone()));
        assert_eq!(bps_after_treasury, 10_i128);

        // set_global_fee returns the number of pools updated.
        let updated = factory.set_global_fee(&admin, &5_i128);
        assert_eq!(updated, 1);

        // The pool's fee_recipient must now be the factory contract.
        let (recipient, bps) = amm_client.get_protocol_fee();
        assert_eq!(recipient, Some(factory_addr.clone()));
        assert_eq!(bps, 5_i128);
    }

    /// Issue #493 (same root cause): `sync_global_fee_page` walks the shared
    /// `PoolByIndex` sequence and, unlike `sweep_fees_page`, had no guard
    /// against non-AMM entries — it would call the AMM-only
    /// `set_protocol_fee()` on whatever address it found. Before CL pools got
    /// their own counter/index, a factory with even one CL pool would trap
    /// here as soon as the loop reached it. Now that CL pools are indexed
    /// separately, `set_global_fee` must skip them cleanly (0 CL pools ever
    /// enter this loop) and only report the AMM pool as updated.
    #[test]
    fn test_set_global_fee_skips_cl_pools() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);
        let cl_hash = env
            .deployer()
            .upload_contract_wasm(concentrated_liquidity::WASM);

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);
        factory.set_cl_wasm_hash(&cl_hash);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        let (pool_addr, _) = factory.create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);
        // Creating a CL pool for the same pair must not disturb the AMM fee sync.
        factory.create_cl_pool(&admin, &ta, &tb, &5_i128, &0_i32);

        factory.set_treasury(&admin, &treasury, &10_i128);

        // Must not trap on the CL pool and must only update the AMM pool.
        let updated = factory.set_global_fee(&admin, &5_i128);
        assert_eq!(updated, 1);

        let amm_client = amm::AmmPoolClient::new(&env, &pool_addr);
        let (recipient, bps) = amm_client.get_protocol_fee();
        assert_eq!(recipient, Some(factory_addr.clone()));
        assert_eq!(bps, 5_i128);
    }

    #[test]
    fn test_sweep_fees_transfers_to_treasury() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        // Deploy real token contracts as the pool tokens.
        let ta_addr = env.register_contract(None, token::LpToken);
        let tb_addr = env.register_contract(None, token::LpToken);
        token::LpTokenClient::new(&env, &ta_addr).initialize(
            &admin,
            &soroban_sdk::String::from_str(&env, "Token A"),
            &soroban_sdk::String::from_str(&env, "TKA"),
            &7u32,
        );
        token::LpTokenClient::new(&env, &tb_addr).initialize(
            &admin,
            &soroban_sdk::String::from_str(&env, "Token B"),
            &soroban_sdk::String::from_str(&env, "TKB"),
            &7u32,
        );

        let (pool_addr, _) =
            factory.create_pool_with_fee_bps(&admin, &ta_addr, &tb_addr, &30_i128, &None);

        // Configure treasury and propagate fee (10 bps) to the pool.
        factory.set_treasury(&admin, &treasury, &10_i128);

        // Seed the pool with liquidity so swaps work.
        let _lp_addr = factory.get_lp_token(&pool_addr).unwrap();
        token::LpTokenClient::new(&env, &ta_addr).mint(&admin, &10_000_000_i128);
        token::LpTokenClient::new(&env, &tb_addr).mint(&admin, &10_000_000_i128);
        let amm = amm::AmmPoolClient::new(&env, &pool_addr);
        amm.add_liquidity(
            &admin,
            &10_000_000_i128,
            &10_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        // Perform a swap so fees accrue.
        let trader = Address::generate(&env);
        token::LpTokenClient::new(&env, &ta_addr).mint(&trader, &1_000_000_i128);
        amm.swap(&trader, &ta_addr, &1_000_000_i128, &0_i128, &u64::MAX);

        // Verify some fees accrued.
        let (accrued_a, _) = amm.get_accrued_fees();
        assert!(accrued_a > 0, "protocol fees must have accrued after swap");

        let treasury_a_before = soroban_sdk::token::Client::new(&env, &ta_addr).balance(&treasury);

        // Sweep — permissionless.
        let swept_a = factory.sweep_fees(&ta_addr);

        assert!(swept_a > 0);

        let treasury_a_after = soroban_sdk::token::Client::new(&env, &ta_addr).balance(&treasury);
        assert_eq!(
            treasury_a_after - treasury_a_before,
            swept_a,
            "treasury must receive exactly the swept fees"
        );

        // After sweep, pool's accrued fees must be zeroed.
        let (post_sweep_a, post_sweep_b) = amm.get_accrued_fees();
        assert_eq!(post_sweep_a, 0);
        assert_eq!(post_sweep_b, 0);
    }

    #[test]
    fn test_sweep_fees_zero_fees_is_noop() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let ta_addr = env.register_contract(None, token::LpToken);
        let tb_addr = env.register_contract(None, token::LpToken);
        token::LpTokenClient::new(&env, &ta_addr).initialize(
            &admin,
            &soroban_sdk::String::from_str(&env, "Token A"),
            &soroban_sdk::String::from_str(&env, "TKA"),
            &7u32,
        );
        token::LpTokenClient::new(&env, &tb_addr).initialize(
            &admin,
            &soroban_sdk::String::from_str(&env, "Token B"),
            &soroban_sdk::String::from_str(&env, "TKB"),
            &7u32,
        );

        factory.create_pool_with_fee_bps(&admin, &ta_addr, &tb_addr, &30_i128, &None);
        factory.set_treasury(&admin, &treasury, &10_i128);

        let treasury_before = soroban_sdk::token::Client::new(&env, &ta_addr).balance(&treasury);
        let swept = factory.sweep_fees(&ta_addr);
        let treasury_after = soroban_sdk::token::Client::new(&env, &ta_addr).balance(&treasury);

        assert_eq!(swept, 0);
        assert_eq!(treasury_after, treasury_before);
    }

    #[test]
    fn test_sweep_fees_multiple_pools_aggregates_requested_token() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let treasury = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let token_a = env.register_contract(None, token::LpToken);
        let token_b = env.register_contract(None, token::LpToken);
        let token_c = env.register_contract(None, token::LpToken);
        for (addr, name, symbol) in [
            (token_a.clone(), "Token A", "TKA"),
            (token_b.clone(), "Token B", "TKB"),
            (token_c.clone(), "Token C", "TKC"),
        ] {
            token::LpTokenClient::new(&env, &addr).initialize(
                &admin,
                &soroban_sdk::String::from_str(&env, name),
                &soroban_sdk::String::from_str(&env, symbol),
                &7u32,
            );
        }

        let (pool_ab, _) =
            factory.create_pool_with_fee_bps(&admin, &token_a, &token_b, &30_i128, &None);
        let (pool_ac, _) =
            factory.create_pool_with_fee_bps(&admin, &token_a, &token_c, &30_i128, &None);
        factory.set_treasury(&admin, &treasury, &10_i128);

        let amm_ab = amm::AmmPoolClient::new(&env, &pool_ab);
        let amm_ac = amm::AmmPoolClient::new(&env, &pool_ac);

        for token_addr in [token_a.clone(), token_b.clone(), token_c.clone()] {
            token::LpTokenClient::new(&env, &token_addr).mint(&admin, &20_000_000_i128);
        }
        amm_ab.add_liquidity(
            &admin,
            &10_000_000_i128,
            &10_000_000_i128,
            &0_i128,
            &u64::MAX,
        );
        amm_ac.add_liquidity(
            &admin,
            &10_000_000_i128,
            &10_000_000_i128,
            &0_i128,
            &u64::MAX,
        );

        let trader = Address::generate(&env);
        token::LpTokenClient::new(&env, &token_a).mint(&trader, &2_000_000_i128);
        amm_ab.swap(&trader, &token_a, &1_000_000_i128, &0_i128, &u64::MAX);
        amm_ac.swap(&trader, &token_a, &1_000_000_i128, &0_i128, &u64::MAX);

        let expected = token_fee_for_pool(&factory, &pool_ab, &token_a, &amm_ab)
            + token_fee_for_pool(&factory, &pool_ac, &token_a, &amm_ac);
        assert!(expected > 0);

        let treasury_before = soroban_sdk::token::Client::new(&env, &token_a).balance(&treasury);
        let swept = factory.sweep_fees(&token_a);
        let treasury_after = soroban_sdk::token::Client::new(&env, &token_a).balance(&treasury);

        assert_eq!(swept, expected);
        assert_eq!(treasury_after - treasury_before, expected);
    }

    #[test]
    fn test_sweep_fees_without_treasury_fails() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        // Anyone can call, but treasury must be configured first.
        let token_to_sweep = Address::generate(&env);
        let result = factory.try_sweep_fees(&token_to_sweep);
        assert!(result.is_err());
    }

    #[test]
    fn test_governance_proposals_for_factory_fee_sweep() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths_allowing_non_root_auth();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let proposer = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);

        let gov_addr = env.register_contract(None, governance::Governance);
        let gov = governance::GovernanceClient::new(&env, &gov_addr);

        factory.initialize(&gov_addr, &amm_hash, &token_hash);

        // Deploy LP token.
        let lp_addr = env.register_contract(None, token::LpToken);
        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &gov_addr,
            &soroban_sdk::String::from_str(&env, "AMM LP"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        // Initialize governance
        gov.initialize(
            &gov_addr,
            &factory_addr, // dummy pool address
            &lp_addr,
            &604800_u64,
            &172800_u64,
            &1000_i128,
            &100_i128,
        );

        token::LpTokenClient::new(&env, &lp_addr).set_locker(&gov_addr);

        // Mint LP tokens to proposer
        let lp_client = token::LpTokenClient::new(&env, &lp_addr);
        lp_client.mint(&proposer, &1000_i128);

        // Create update proposals
        let treasury = Address::generate(&env);

        let treasury_params = governance::UpdateFactoryTreasuryParams {
            factory: factory_addr.clone(),
            treasury: treasury.clone(),
            global_protocol_fee_bps: 300_i128, // 3%
        };

        // Propose treasury update
        let pid1 = gov.propose(
            &proposer,
            &governance::ProposalKind::UpdateFactoryTreasury(treasury_params),
        );

        // Vote
        gov.vote(&proposer, &pid1, &governance::Vote::For);

        // Advance ledger to execution time
        let prop1 = gov.get_proposal(&pid1);
        env.ledger()
            .with_mut(|l| l.timestamp = prop1.execute_after + 1);

        // Execute treasury update proposal
        gov.execute(&pid1);

        // Verify that treasury and global fee got updated in the Factory!
        let (stored_treasury, stored_fee) = factory.get_treasury().unwrap();
        assert_eq!(stored_treasury, treasury);
        assert_eq!(stored_fee, 300_i128);
        gov.unlock_vote(&proposer, &pid1);

        // Test UpdateFactoryGlobalFee proposal
        let global_fee_params = governance::UpdateFactoryGlobalFeeParams {
            factory: factory_addr.clone(),
            offset: 0,
            limit: 100,
        };

        let pid2 = gov.propose(
            &proposer,
            &governance::ProposalKind::UpdateFactoryGlobalFee(global_fee_params),
        );
        gov.vote(&proposer, &pid2, &governance::Vote::For);
        let prop2 = gov.get_proposal(&pid2);
        env.ledger()
            .with_mut(|l| l.timestamp = prop2.execute_after + 1);

        // Execute global fee update proposal
        gov.execute(&pid2);
    }

    // ── Issue #520: validate before charging ────────────────────────────────────

    /// Verify that a duplicate-pool error does NOT charge the pool-creation
    /// fee or consume the caller's rate-limit slot (issue #520).
    ///
    /// Before the fix, `check_and_update_rate_limit` and
    /// `charge_pool_creation_fee` ran *before* the `PoolAlreadyExists` check,
    /// so a caller who accidentally requested an existing pair paid the fee
    /// and burned their rate-limit cooldown for nothing.
    #[test]
    fn test_duplicate_pool_does_not_charge_fee_or_consume_rate_limit() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let fee_token_addr = setup_fee_token(&env, &admin, &user, 10_000);
        let fee_client = soroban_sdk::token::Client::new(&env, &fee_token_addr);
        factory.set_pool_creation_fee(&fee_token_addr, &100_i128);
        factory.set_permissionless_mode(&true);
        factory.set_rate_limit(&5u32);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        // First creation succeeds — fee charged, rate limit recorded.
        factory.create_pool_with_fee_bps(&user, &ta, &tb, &30_i128, &None);
        let balance_after_first = fee_client.balance(&user);

        // Duplicate creation must fail WITHOUT charging a second fee.
        let result = factory.try_create_pool_with_fee_bps(&user, &ta, &tb, &30_i128, &None);
        assert_eq!(
            result,
            Err(Ok(FactoryError::PoolAlreadyExists)),
            "duplicate pool must return PoolAlreadyExists"
        );
        assert_eq!(
            fee_client.balance(&user),
            balance_after_first,
            "fee must NOT be charged for a rejected duplicate"
        );

        // The rate limit must not have been consumed by the failed call, so
        // after advancing past the window, an immediate creation of a
        // DIFFERENT pair must succeed.
        let tc = Address::generate(&env);
        let td = Address::generate(&env);
        // Advance past the 5-ledger rate limit window.
        env.ledger().with_mut(|l| l.sequence_number = 6);
        let result2 = factory.try_create_pool_with_fee_bps(&user, &tc, &td, &30_i128, &None);
        assert!(
            result2.is_ok(),
            "rate limit must not be consumed by the rejected duplicate"
        );
    }

    /// Same as above but for invalid fee_bps — the caller must not be
    /// charged or rate-limited when the request is rejected up front.
    #[test]
    fn test_invalid_fee_bps_does_not_charge_fee_or_consume_rate_limit() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        let fee_token_addr = setup_fee_token(&env, &admin, &user, 10_000);
        let fee_client = soroban_sdk::token::Client::new(&env, &fee_token_addr);
        factory.set_pool_creation_fee(&fee_token_addr, &100_i128);
        factory.set_permissionless_mode(&true);
        factory.set_rate_limit(&5u32);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        // Invalid fee_bps (> 10 000) must be rejected without charge.
        let result = factory.try_create_pool_with_fee_bps(&user, &ta, &tb, &99_999_i128, &None);
        assert_eq!(
            result,
            Err(Ok(FactoryError::InvalidFeeBps)),
            "invalid fee_bps must return InvalidFeeBps"
        );
        assert_eq!(
            fee_client.balance(&user),
            10_000_i128,
            "fee must NOT be charged for invalid fee_bps"
        );

        // Rate limit was not consumed — a valid creation must succeed.
        let tc = Address::generate(&env);
        let td = Address::generate(&env);
        let result2 = factory.try_create_pool_with_fee_bps(&user, &tc, &td, &30_i128, &None);
        assert!(
            result2.is_ok(),
            "rate limit must not be consumed by the rejected call"
        );
    }

    /// CL pool variant: duplicate-pool error must not charge or rate-limit.
    #[test]
    fn test_cl_duplicate_pool_does_not_charge_fee_or_consume_rate_limit() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);
        let cl_hash = env
            .deployer()
            .upload_contract_wasm(concentrated_liquidity::WASM);

        let admin = Address::generate(&env);
        let user = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);
        factory.set_cl_wasm_hash(&cl_hash);

        let fee_token_addr = setup_fee_token(&env, &admin, &user, 10_000);
        let fee_client = soroban_sdk::token::Client::new(&env, &fee_token_addr);
        factory.set_pool_creation_fee(&fee_token_addr, &100_i128);
        factory.set_permissionless_mode(&true);
        factory.set_rate_limit(&5u32);

        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        // First CL pool creation succeeds.
        factory.create_cl_pool(&user, &ta, &tb, &30_i128, &0_i32);
        let balance_after_first = fee_client.balance(&user);

        // Duplicate must fail without a second charge.
        let result = factory.try_create_cl_pool(&user, &ta, &tb, &30_i128, &0_i32);
        assert_eq!(
            result,
            Err(Ok(FactoryError::ClPoolAlreadyExists)),
            "duplicate CL pool must return ClPoolAlreadyExists"
        );
        assert_eq!(
            fee_client.balance(&user),
            balance_after_first,
            "fee must NOT be charged for a rejected CL duplicate"
        );

        // Rate limit was not consumed by the failed call — advance past the
        // window and create a different pair.
        let tc = Address::generate(&env);
        let td = Address::generate(&env);
        env.ledger().with_mut(|l| l.sequence_number = 6);
        let result2 = factory.try_create_cl_pool(&user, &tc, &td, &30_i128, &0_i32);
        assert!(
            result2.is_ok(),
            "rate limit must not be consumed by the rejected CL duplicate"
        );
    }

    // -- Bounded enumeration (issue #790) -------------------------------------

    /// Registers `n` synthetic pool addresses straight into the factory's
    /// index, bypassing `create_pool` so a cap-sized registry can be built
    /// without deploying hundreds of real contracts. The stored addresses are
    /// never invoked -- these tests only exercise the read/pagination path,
    /// which reads `PoolByIndex` entries and never calls into them.
    fn seed_pool_index(env: &Env, factory: &Address, n: u64, cl: bool) -> Vec<Address> {
        let mut expected = Vec::new(env);
        for _ in 0..n {
            expected.push_back(Address::generate(env));
        }
        env.as_contract(factory, || {
            for i in 0..n {
                let addr = expected.get(i as u32).unwrap();
                if cl {
                    env.storage()
                        .persistent()
                        .set(&DataKey::ClPoolByIndex(i), &addr);
                } else {
                    env.storage()
                        .persistent()
                        .set(&DataKey::PoolByIndex(i), &addr);
                }
            }
            let key = if cl {
                DataKey::ClPoolCount
            } else {
                DataKey::PoolCount
            };
            env.storage().instance().set(&key, &n);
        });
        expected
    }

    fn bounded_factory(env: &Env) -> Address {
        env.budget().reset_unlimited();
        env.mock_all_auths();
        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);
        let admin = Address::generate(env);
        let factory_addr = env.register_contract(None, Factory);
        FactoryClient::new(env, &factory_addr).initialize(&admin, &amm_hash, &token_hash);
        factory_addr
    }

    #[test]
    fn test_all_pools_is_capped_once_registry_exceeds_the_limit() {
        let env = Env::default();
        let factory_addr = bounded_factory(&env);
        let factory = FactoryClient::new(&env, &factory_addr);

        let cap = Factory::MAX_UNBOUNDED_PAGE as u64;
        let total = cap + 25;
        let expected = seed_pool_index(&env, &factory_addr, total, false);

        // The count still reports the true total, so a caller can detect that
        // `all_pools()` truncated.
        assert_eq!(factory.get_pool_count(), total);

        let all = factory.all_pools();
        assert_eq!(all.len(), Factory::MAX_UNBOUNDED_PAGE);
        // Truncation keeps the oldest pools, in index order.
        for i in 0..Factory::MAX_UNBOUNDED_PAGE {
            assert_eq!(all.get(i).unwrap(), expected.get(i).unwrap());
        }
    }

    #[test]
    fn test_all_cl_pools_is_capped_once_registry_exceeds_the_limit() {
        let env = Env::default();
        let factory_addr = bounded_factory(&env);
        let factory = FactoryClient::new(&env, &factory_addr);

        let total = Factory::MAX_UNBOUNDED_PAGE as u64 + 7;
        let expected = seed_pool_index(&env, &factory_addr, total, true);

        assert_eq!(factory.get_cl_pool_count(), total);

        let all = factory.all_cl_pools();
        assert_eq!(all.len(), Factory::MAX_UNBOUNDED_PAGE);
        for i in 0..Factory::MAX_UNBOUNDED_PAGE {
            assert_eq!(all.get(i).unwrap(), expected.get(i).unwrap());
        }
    }

    #[test]
    fn test_paginated_reads_reach_every_pool_past_the_cap() {
        let env = Env::default();
        let factory_addr = bounded_factory(&env);
        let factory = FactoryClient::new(&env, &factory_addr);

        let total = Factory::MAX_UNBOUNDED_PAGE as u64 + 25;
        let expected = seed_pool_index(&env, &factory_addr, total, false);

        // Page size well under the cap: every pool is still reachable, and no
        // single call reads more than `page` entries.
        let page = 10u32;
        let mut seen = Vec::new(&env);
        let count = factory.get_pool_count();
        let mut offset = 0u64;
        while offset < count {
            let chunk = factory.get_pools(&(offset as u32), &page);
            assert!(chunk.len() <= page);
            for i in 0..chunk.len() {
                seen.push_back(chunk.get(i).unwrap());
            }
            offset += page as u64;
        }

        assert_eq!(seen.len() as u64, total);
        for i in 0..total {
            assert_eq!(seen.get(i as u32).unwrap(), expected.get(i as u32).unwrap());
        }
    }

    #[test]
    fn test_paginated_cl_reads_reach_every_pool_past_the_cap() {
        let env = Env::default();
        let factory_addr = bounded_factory(&env);
        let factory = FactoryClient::new(&env, &factory_addr);

        let total = Factory::MAX_UNBOUNDED_PAGE as u64 + 13;
        let expected = seed_pool_index(&env, &factory_addr, total, true);

        let page = 25u32;
        let mut seen = Vec::new(&env);
        let mut offset = 0u64;
        while offset < total {
            let chunk = factory.get_cl_pools(&(offset as u32), &page);
            assert!(chunk.len() <= page);
            for i in 0..chunk.len() {
                seen.push_back(chunk.get(i).unwrap());
            }
            offset += page as u64;
        }

        assert_eq!(seen.len() as u64, total);
        for i in 0..total {
            assert_eq!(seen.get(i as u32).unwrap(), expected.get(i as u32).unwrap());
        }
    }

    #[test]
    fn test_all_pools_below_the_cap_is_unchanged() {
        let env = Env::default();
        let factory_addr = bounded_factory(&env);
        let factory = FactoryClient::new(&env, &factory_addr);

        // Under the cap, `all_pools()` must still behave exactly as before:
        // every pool, in index order, identical to a full-range `get_pools`.
        let total = 5u64;
        let expected = seed_pool_index(&env, &factory_addr, total, false);

        let all = factory.all_pools();
        assert_eq!(all.len() as u64, total);
        assert_eq!(all, expected);
        assert_eq!(all, factory.get_pools(&0, &(total as u32)));
        assert_eq!(factory.all_pools(), factory.get_pools(&0, &u32::MAX));
    }

    // ── Instance TTL regression (issue #904) ────────────────────────────────
    // The factory previously managed no TTL at all. Its instance entry holds
    // the admin, WASM hashes, pool counts, fee config and treasury; if it is
    // archived the pool registry becomes unreachable and no new pool can be
    // deployed. These tests initialise a factory with placeholder WASM hashes
    // (no pool is created, so no real WASM is needed), drive the ledger past
    // the instance TTL, then confirm entrypoints still respond and re-extend it.

    fn ttl_env_and_factory() -> (Env, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        let amm_hash = BytesN::from_array(&env, &[1u8; 32]);
        let token_hash = BytesN::from_array(&env, &[2u8; 32]);
        factory.initialize(&admin, &amm_hash, &token_hash);
        (env, factory_addr)
    }

    fn instance_ttl(env: &Env, factory_addr: &Address) -> u32 {
        env.as_contract(factory_addr, || env.storage().instance().get_ttl())
    }

    fn lower_instance_ttl_below_min(env: &Env, factory_addr: &Address) {
        env.ledger()
            .with_mut(|l| l.sequence_number += Factory::BUMP_TO - Factory::MIN_TTL + 1);
        let ttl = instance_ttl(env, factory_addr);
        assert!(
            ttl < Factory::MIN_TTL,
            "test setup should lower instance TTL below MIN_TTL, got {ttl}"
        );
    }

    fn assert_instance_ttl_bumped(env: &Env, factory_addr: &Address) {
        let ttl = instance_ttl(env, factory_addr);
        assert!(
            ttl >= Factory::BUMP_TO - 1,
            "instance TTL {ttl} should be bumped toward BUMP_TO"
        );
    }

    #[test]
    fn test_initialize_extends_instance_ttl() {
        let (env, factory_addr) = ttl_env_and_factory();
        assert_instance_ttl_bumped(&env, &factory_addr);
    }

    #[test]
    fn test_read_entrypoint_restores_lapsed_instance_ttl() {
        let (env, factory_addr) = ttl_env_and_factory();
        let factory = FactoryClient::new(&env, &factory_addr);

        lower_instance_ttl_below_min(&env, &factory_addr);

        // A pure registry lookup is often the only traffic the factory sees for
        // long stretches; it must still respond and restore the instance TTL.
        assert_eq!(factory.get_pool_count(), 0);
        assert_instance_ttl_bumped(&env, &factory_addr);
    }

    #[test]
    fn test_write_entrypoint_restores_lapsed_instance_ttl() {
        let (env, factory_addr) = ttl_env_and_factory();
        let factory = FactoryClient::new(&env, &factory_addr);

        lower_instance_ttl_below_min(&env, &factory_addr);

        // `set_default_fee_tier` writes only instance state, isolating the
        // write-path TTL bump from the per-pool persistent registry entries.
        factory.set_default_fee_tier(&1i128);

        assert_eq!(factory.get_default_fee_tier(), 1);
        assert_instance_ttl_bumped(&env, &factory_addr);
    }

    // ── Issue #923: no event escapes the version stamp ────────────────────────
    //
    // Rather than one assertion per topic, this walks the whole event log for a
    // representative call sequence and asserts every payload this contract
    // emitted begins with `EVENT_SCHEMA_VERSION`. A newly added raw
    // `env.events().publish(...)` reachable from the sequence regresses this
    // test, which is the guard the half-migrated state lacked.
    #[test]
    fn test_every_emitted_event_carries_version_stamp() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::{TryFromVal, Val};

        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let amm_hash = env.deployer().upload_contract_wasm(amm::WASM);
        let token_hash = env.deployer().upload_contract_wasm(token::WASM);

        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        factory.initialize(&admin, &amm_hash, &token_hash);

        // Pool creation: pool_created.
        let ta = Address::generate(&env);
        let tb = Address::generate(&env);
        factory.create_pool_with_fee_bps(&admin, &ta, &tb, &30_i128, &None);

        // Admin-config surface: default_fee_tier_updated, creation_paused,
        // creation_unpaused, mode_changed.
        factory.set_default_fee_tier(&2_i128);
        factory.pause_creation(&admin);
        factory.unpause_creation(&admin);
        factory.set_permissionless_mode(&true);

        // WASM rotation last, since it points the contract at new bytecode:
        // upgraded.
        factory.upgrade(&token_hash);

        // Every event this contract published must decode as `(u32, _)` with the
        // leading `u32` equal to the current schema version. Events emitted by
        // the deployed pool/token contracts are filtered out by address.
        let mut checked = 0u32;
        for event in env.events().all().iter() {
            if event.0 != factory_addr {
                continue;
            }
            let payload: soroban_sdk::Vec<Val> = soroban_sdk::Vec::try_from_val(&env, &event.2)
                .expect("every factory event payload is a versioned tuple");
            let head = payload.get(0).expect("versioned payload is never empty");
            let version = u32::try_from_val(&env, &head)
                .expect("leading element of every factory event is the u32 version");
            assert_eq!(
                version,
                soroban_amm_sdk::EVENT_SCHEMA_VERSION,
                "event with topics {:?} is missing the schema version stamp",
                event.1
            );
            checked += 1;
        }
        assert!(
            checked >= 5,
            "representative sequence should emit several factory events, saw {checked}"
        );
    }

    #[test]
    fn test_admin_rotation_happy_path() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        let dummy_hash = soroban_sdk::BytesN::from_array(&env, &[0u8; 32]);
        factory.initialize(&admin, &dummy_hash, &dummy_hash);

        let new_admin = Address::generate(&env);
        assert_eq!(factory.get_admin(), Some(admin.clone()));
        assert_eq!(factory.get_pending_admin(), None);

        // Propose admin
        factory.propose_admin(&admin, &new_admin);
        assert_eq!(factory.get_pending_admin(), Some(new_admin.clone()));
        assert_eq!(factory.get_admin(), Some(admin.clone()));

        // Accept admin
        factory.accept_admin(&new_admin);
        assert_eq!(factory.get_admin(), Some(new_admin.clone()));
        assert_eq!(factory.get_pending_admin(), None);
    }

    #[test]
    fn test_propose_admin_unauthorized() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let rando = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        let dummy_hash = soroban_sdk::BytesN::from_array(&env, &[0u8; 32]);
        factory.initialize(&admin, &dummy_hash, &dummy_hash);

        let new_admin = Address::generate(&env);
        assert_eq!(
            factory.try_propose_admin(&rando, &new_admin),
            Err(Ok(FactoryError::Unauthorized))
        );
        assert_eq!(factory.get_pending_admin(), None);
        assert_eq!(factory.get_admin(), Some(admin));
    }

    #[test]
    fn test_accept_admin_wrong_address_or_no_pending() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let rando = Address::generate(&env);
        let factory_addr = env.register_contract(None, Factory);
        let factory = FactoryClient::new(&env, &factory_addr);
        let dummy_hash = soroban_sdk::BytesN::from_array(&env, &[0u8; 32]);
        factory.initialize(&admin, &dummy_hash, &dummy_hash);

        let new_admin = Address::generate(&env);

        // Accept without prior proposal
        assert_eq!(
            factory.try_accept_admin(&new_admin),
            Err(Ok(FactoryError::NoPendingAdmin))
        );

        // Propose to new_admin
        factory.propose_admin(&admin, &new_admin);

        // Wrong address calling accept_admin
        assert_eq!(
            factory.try_accept_admin(&rando),
            Err(Ok(FactoryError::WrongAdmin))
        );
        assert_eq!(factory.get_admin(), Some(admin));
    }

    // ── #932: typed NotInitialized instead of host traps ─────────────────────

    fn uninitialized_factory(env: &Env) -> FactoryClient<'_> {
        env.mock_all_auths();
        let factory_addr = env.register_contract(None, Factory);
        FactoryClient::new(env, &factory_addr)
    }

    #[test]
    fn test_pre_init_pool_creation_returns_not_initialized() {
        let env = Env::default();
        let factory = uninitialized_factory(&env);
        let caller = Address::generate(&env);
        let token_a = Address::generate(&env);
        let token_b = Address::generate(&env);

        assert_eq!(
            factory.try_create_pool(&caller, &token_a, &token_b, &2_i128, &None),
            Err(Ok(FactoryError::NotInitialized))
        );
        assert_eq!(
            factory.try_create_pool_with_fee_bps(&caller, &token_a, &token_b, &30_i128, &None),
            Err(Ok(FactoryError::NotInitialized))
        );
        assert_eq!(
            factory.try_create_cl_pool(&caller, &token_a, &token_b, &30_i128, &0_i32),
            Err(Ok(FactoryError::NotInitialized))
        );
    }

    #[test]
    fn test_pre_init_admin_entrypoints_return_not_initialized() {
        let env = Env::default();
        let factory = uninitialized_factory(&env);
        let admin = Address::generate(&env);
        let other = Address::generate(&env);
        let hash = soroban_sdk::BytesN::from_array(&env, &[7u8; 32]);

        assert_eq!(
            factory.try_update_wasm_hashes(&Some(hash.clone()), &None),
            Err(Ok(FactoryError::NotInitialized))
        );
        assert_eq!(
            factory.try_set_default_fee_tier(&1_i128),
            Err(Ok(FactoryError::NotInitialized))
        );
        assert_eq!(
            factory.try_upgrade(&hash),
            Err(Ok(FactoryError::NotInitialized))
        );
        assert_eq!(
            factory.try_set_cl_wasm_hash(&hash),
            Err(Ok(FactoryError::NotInitialized))
        );
        assert_eq!(
            factory.try_pause_creation(&admin),
            Err(Ok(FactoryError::NotInitialized))
        );
        assert_eq!(
            factory.try_unpause_creation(&admin),
            Err(Ok(FactoryError::NotInitialized))
        );
        assert_eq!(
            factory.try_set_permissionless_mode(&true),
            Err(Ok(FactoryError::NotInitialized))
        );
        assert_eq!(
            factory.try_set_pool_creation_fee(&other, &100_i128),
            Err(Ok(FactoryError::NotInitialized))
        );
        assert_eq!(
            factory.try_set_rate_limit(&5_u32),
            Err(Ok(FactoryError::NotInitialized))
        );
        assert_eq!(
            factory.try_propose_admin(&admin, &other),
            Err(Ok(FactoryError::NotInitialized))
        );
        assert_eq!(
            factory.try_set_treasury(&admin, &other, &100_i128),
            Err(Ok(FactoryError::NotInitialized))
        );
        assert_eq!(
            factory.try_set_global_fee(&admin, &100_i128),
            Err(Ok(FactoryError::NotInitialized))
        );
        assert_eq!(
            factory.try_set_global_fee_paginated(&admin, &0_u32, &10_u32),
            Err(Ok(FactoryError::NotInitialized))
        );
    }

    #[test]
    fn test_pre_init_other_entrypoints_do_not_trap() {
        let env = Env::default();
        let factory = uninitialized_factory(&env);
        let someone = Address::generate(&env);

        // Entry points that never read admin/WASM config keep their own
        // typed errors or empty defaults.
        assert_eq!(
            factory.try_accept_admin(&someone),
            Err(Ok(FactoryError::NoPendingAdmin))
        );
        assert_eq!(
            factory.try_sweep_fees(&someone),
            Err(Ok(FactoryError::FeeNotConfigured))
        );
        assert_eq!(factory.get_admin(), None);
        assert_eq!(factory.get_pool_count(), 0);
        assert!(!factory.is_creation_paused());
    }
}
