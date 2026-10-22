//! MEV-resistant batch auction contract.
//!
//! Collects swap orders within a configurable time window and settles them
//! atomically in a single transaction. Because no external trade can be
//! inserted between batched orders during settlement, sandwich attacks are
//! structurally impossible for orders in the same batch window.
//!
//! Flow:
//!   1. Deploy and `initialize` with an admin and a batch window duration.
//!   2. Traders call `submit_order` — tokens are escrowed immediately until
//!      the current batch reaches the configured order cap.
//!   3. After the window elapses, anyone calls `settle_batch`.
//!   4. Settlement executes all orders atomically; output tokens go to traders.
//!   5. Any trader may call `cancel_order` before settlement to reclaim tokens.

#![no_std]

use pool_interfaces::{AmmPoolClient, ConcentratedLiquidityClient, FactoryClient};
use soroban_amm_sdk::emit_versioned_event;
use soroban_sdk::token::Client as SepTokenClient;
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Env, Symbol, Vec,
};

const DEFAULT_MAX_ORDERS: u32 = 50;
const MAX_ORDERS_CEILING: u32 = 200;
/// Ceiling on the admin-managed venue allowlist (issue #700): `VenueList` is
/// read and rewritten in full on every `add_venue`/`remove_venue` call, so an
/// unbounded list would eventually make those calls exceed the ledger
/// footprint/resource limits. `remove_venue` a stale entry to add another
/// past this cap.
const MAX_VENUES: u32 = 200;
/// Ceiling on how far in the future a trader-supplied `deadline` may be
/// (issue #700): otherwise a trader could pin their escrow open indefinitely
/// with a far-future deadline, since only the trader's own `cancel_order` (or
/// this ceiling) can release it before then.
const MAX_ORDER_LIFETIME_SECS: u64 = 7 * 24 * 60 * 60;

/// Default lower bound for the batch window (issue #1119): a window shorter
/// than this is effectively no batching, so a sandwich fits between two
/// settlements. The admin can move the bounds with `set_window_bounds`.
pub const MIN_BATCH_WINDOW_SECS: u64 = 5;
/// Default upper bound for the batch window (issue #1119): one day. A larger
/// window stalls settlement for as long as it is configured.
pub const MAX_BATCH_WINDOW_SECS: u64 = 24 * 60 * 60;
/// Hard ceiling for the *upper* bound the admin may configure (issue #1119),
/// so `opened_at + window` can never get near `u64::MAX` whatever is set.
pub const WINDOW_BOUNDS_CEILING_SECS: u64 = 30 * 24 * 60 * 60;

const MIN_TTL: u32 = 172_800;
const BUMP_TO: u32 = 518_400;

/// Persistent-storage TTL for orders, claimables and the venue registry.
/// Must outlive `MAX_ORDER_LIFETIME_SECS` plus a claim window, so a trader
/// can always `claim_refund` a stranded refund long after the order expired.
const PERSISTENT_MIN_TTL: u32 = 172_800;
const PERSISTENT_BUMP_TO: u32 = 3_110_400;

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum AuctionError {
    AlreadyInitialized = 1,
    Unauthorized = 2,
    OrderNotFound = 3,
    BatchWindowOpen = 4,
    NoOrders = 5,
    ZeroAmount = 6,
    DeadlineExceeded = 7,
    BatchFull = 8,
    InvalidMaxOrders = 9,
    /// `token_in`/`token_out` do not match the pool's token pair (issue #361).
    InvalidPoolTokenPair = 10,
    /// Token transfer to/from a trader failed (issue #546).
    TransferFailed = 11,
    /// `accept_admin` called without a prior `propose_admin` (issue #553).
    NoPendingAdmin = 12,
    /// `accept_admin` caller is not the pending nominee (issue #553).
    WrongAdmin = 13,
    /// `pool`/`alt_pool` is neither on the admin allowlist nor attested to by
    /// the configured factory (issue #700).
    UnknownVenue = 14,
    /// An order's venue was allow-listed at submission but has since been
    /// removed; the order is refunded at settlement instead of executed
    /// against it (issue #700).
    VenueRemoved = 15,
    /// `deadline` is further out than `MAX_ORDER_LIFETIME_SECS` (issue #700).
    DeadlineTooFar = 16,
    /// `expire_order` called on an order whose deadline has not passed yet
    /// (issue #700).
    OrderNotExpired = 17,
    /// `claim_refund` called for an order with no claimable balance on
    /// record (issue #700).
    NothingToClaim = 18,
    /// `add_venue` would push the persistent venue registry past
    /// `MAX_VENUES` (issue #700).
    TooManyVenues = 19,
    /// An admin entrypoint, `get_admin` or `accept_admin` was called before
    /// `initialize` (issue #1119).
    NotInitialized = 20,
    /// `batch_window_secs` is outside the configured `[min, max]` window
    /// bounds (issue #1119).
    InvalidBatchWindow = 21,
    /// `set_window_bounds` was given `min == 0`, `min > max`, a `max` above
    /// `WINDOW_BOUNDS_CEILING_SECS`, or bounds that exclude the current window
    /// (issue #1119).
    InvalidWindowBounds = 22,
}

// ── Storage types ─────────────────────────────────────────────────────────────

/// Settlement venue an order may be routed to (issue #351).
///
/// `Amm` dispatches through [`AmmPoolClient`] (constant-product pool); `Cl`
/// dispatches through [`ConcentratedLiquidityClient`] (Uniswap-v3-style
/// concentrated-liquidity pool). Both venues escrow the input from, and pay the
/// output to, the batch-auction contract, so they are interchangeable from a
/// trader's perspective.
#[contracttype]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolType {
    Amm,
    Cl,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct Order {
    pub id: u64,
    pub trader: Address,
    pub pool: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: i128,
    pub min_out: i128,
    pub submitted_at: u64,
    /// Trader-supplied deadline (ledger timestamp). Honored verbatim at
    /// settlement time: if it has passed by the time `settle_batch` runs, the
    /// order is expired and refunded rather than executed against a
    /// freshly-computed deadline.
    pub deadline: u64,
    /// Venue type of `pool`.
    pub pool_type: PoolType,
    /// Swap direction for concentrated-liquidity venues: `true` swaps token A
    /// for token B (price decreasing). Unused for `PoolType::Amm`.
    pub zero_for_one: bool,
    /// `sqrtPriceX96` limit for concentrated-liquidity venues. `0` means the
    /// pool's own default bound is used. Unused for `PoolType::Amm`.
    pub sqrt_price_limit: u128,
    /// Optional alternate venue of the *opposite* `PoolType`, trading the same
    /// `token_in → token_out` pair. When present, settlement quotes both venues
    /// and routes the swap to whichever returns more output (issue #351).
    pub alt_pool: Option<Address>,
}

#[contracttype]
pub enum DataKey {
    Admin,
    /// Nominated admin awaiting `accept_admin` (two-step rotation, issue #553).
    PendingAdmin,
    BatchWindowSecs,
    BatchOpenedAt,
    MaxOrders,
    NextOrderId,
    Order(u64),
    PendingOrders,
    /// The factory that attests to protocol-deployed pools (issue #700).
    Factory,
    /// Admin-managed allowlist entry for a venue the factory doesn't know
    /// about (issue #700).
    Venue(Address),
    /// Enumeration index for `list_venues` (issue #700).
    VenueList,
    /// A stranded `(trader, token, amount)` from a refund/payout transfer
    /// that failed during `settle_batch`/`expire_order`, claimable later via
    /// `claim_refund` (issue #700).
    Claimable(u64),
    /// Admin-settable `(min, max)` bounds for the batch window (issue #1119).
    /// Absent means `(MIN_BATCH_WINDOW_SECS, MAX_BATCH_WINDOW_SECS)`.
    WindowBounds,
}

fn max_orders(env: &Env) -> u32 {
    env.storage()
        .instance()
        .get(&DataKey::MaxOrders)
        .unwrap_or(DEFAULT_MAX_ORDERS)
}

fn extend_ttl(env: &Env) {
    env.storage().instance().extend_ttl(MIN_TTL, BUMP_TO);
}

/// The one place the admin is read (issue #1119): an uninitialized contract
/// returns `NotInitialized` instead of trapping on an `unwrap`.
fn read_admin(env: &Env) -> Result<Address, AuctionError> {
    env.storage()
        .instance()
        .get(&DataKey::Admin)
        .ok_or(AuctionError::NotInitialized)
}

/// The configured `(min, max)` batch-window bounds, or the defaults.
fn window_bounds(env: &Env) -> (u64, u64) {
    env.storage()
        .instance()
        .get(&DataKey::WindowBounds)
        .unwrap_or((MIN_BATCH_WINDOW_SECS, MAX_BATCH_WINDOW_SECS))
}

/// Reject a batch window outside the configured bounds (issue #1119).
fn validate_window(env: &Env, batch_window_secs: u64) -> Result<(), AuctionError> {
    let (min, max) = window_bounds(env);
    if batch_window_secs < min || batch_window_secs > max {
        return Err(AuctionError::InvalidBatchWindow);
    }
    Ok(())
}

/// Bump the TTL of a persistent entry so it outlives the order lifetime plus
/// a claim window. Called on every write and every read of `Order`/`Claimable`.
fn bump_persistent(env: &Env, key: &DataKey) {
    env.storage()
        .persistent()
        .extend_ttl(key, PERSISTENT_MIN_TTL, PERSISTENT_BUMP_TO);
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct BatchAuction;

#[contractimpl]
impl BatchAuction {
    /// Initialize the auction contract.
    ///
    /// - `batch_window_secs` — how long (in ledger seconds) a batch window stays
    ///   open before it can be settled. Must lie within the window bounds
    ///   (`MIN_BATCH_WINDOW_SECS`..=`MAX_BATCH_WINDOW_SECS` until the admin
    ///   changes them), otherwise `InvalidBatchWindow`.
    pub fn initialize(
        env: Env,
        admin: Address,
        batch_window_secs: u64,
    ) -> Result<(), AuctionError> {
        extend_ttl(&env);
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(AuctionError::AlreadyInitialized);
        }
        validate_window(&env, batch_window_secs)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::BatchWindowSecs, &batch_window_secs);
        env.storage().instance().set(&DataKey::NextOrderId, &0_u64);
        env.storage()
            .instance()
            .set(&DataKey::MaxOrders, &DEFAULT_MAX_ORDERS);
        env.storage()
            .instance()
            .set(&DataKey::PendingOrders, &Vec::<u64>::new(&env));
        env.storage()
            .instance()
            .set(&DataKey::BatchOpenedAt, &env.ledger().timestamp());
        Ok(())
    }

    /// Submit a constant-product (AMM) swap order and escrow `amount_in` of
    /// `token_in`.
    ///
    /// Tokens are pulled from `trader` immediately so the batch holds a firm
    /// commitment. `token_in`/`token_out` must be the pool's token pair; this
    /// is validated here so a mismatched order can never reach `settle_batch`.
    ///
    /// Returns the new order ID.
    // Every parameter is required contract-call input; splitting it into a
    // struct would just move the same 8 fields onto the caller's ABI.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_order(
        env: Env,
        trader: Address,
        pool: Address,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
        min_out: i128,
        deadline: u64,
    ) -> Result<u64, AuctionError> {
        extend_ttl(&env);
        Self::record_order(
            env,
            trader,
            pool,
            token_in,
            token_out,
            amount_in,
            min_out,
            deadline,
            PoolType::Amm,
            false,
            0,
            None,
        )
    }

    /// Submit a concentrated-liquidity (CL) swap order and escrow `amount_in`
    /// of `token_in` (issue #351).
    ///
    /// `zero_for_one` selects the CL swap direction (token A → token B when
    /// `true`) and `sqrt_price_limit` is the `sqrtPriceX96` bound passed to the
    /// CL pool (`0` lets the pool walk to its own bound).
    ///
    /// `alt_amm_pool` may name a constant-product pool trading the same
    /// `token_in → token_out` pair. When supplied, settlement quotes both the
    /// CL pool and the AMM pool and routes the swap to whichever returns more
    /// output, giving batched traders best execution across venue types.
    ///
    /// Returns the new order ID.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_order_cl(
        env: Env,
        trader: Address,
        pool: Address,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
        min_out: i128,
        zero_for_one: bool,
        sqrt_price_limit: u128,
        alt_amm_pool: Option<Address>,
        deadline: u64,
    ) -> Result<u64, AuctionError> {
        extend_ttl(&env);
        Self::record_order(
            env,
            trader,
            pool,
            token_in,
            token_out,
            amount_in,
            min_out,
            deadline,
            PoolType::Cl,
            zero_for_one,
            sqrt_price_limit,
            alt_amm_pool,
        )
    }

    /// Shared order-intake path: validate, escrow `amount_in`, persist the
    /// order, and enqueue it into the current batch window.
    #[allow(clippy::too_many_arguments)]
    fn record_order(
        env: Env,
        trader: Address,
        pool: Address,
        token_in: Address,
        token_out: Address,
        amount_in: i128,
        min_out: i128,
        deadline: u64,
        pool_type: PoolType,
        zero_for_one: bool,
        sqrt_price_limit: u128,
        alt_pool: Option<Address>,
    ) -> Result<u64, AuctionError> {
        let now = env.ledger().timestamp();
        if deadline < now {
            return Err(AuctionError::DeadlineExceeded);
        }
        if deadline > now + MAX_ORDER_LIFETIME_SECS {
            return Err(AuctionError::DeadlineTooFar);
        }
        if amount_in <= 0 {
            return Err(AuctionError::ZeroAmount);
        }

        // Validate `pool` is a venue this protocol actually deployed (attested
        // to by the factory) or one the admin has explicitly vetted, and that
        // it trades the order's exact token pair. Trusting the pool's own
        // self-reported token pair alone (the previous check) let an attacker
        // name their own contract as the venue and drain the shared escrow
        // once settle_batch called into it (issue #700).
        Self::check_venue(&env, &pool, pool_type, &token_in, &token_out)?;
        // An alternate venue must be equally legitimate and trade the same
        // pair as the primary venue. Otherwise best-venue routing could quote
        // and select a real pool over `token_in -> token_x`, causing
        // settlement to receive the wrong asset — or a malicious venue.
        if let Some(ref alt) = alt_pool {
            let alt_type = match pool_type {
                PoolType::Amm => PoolType::Cl,
                PoolType::Cl => PoolType::Amm,
            };
            Self::check_venue(&env, alt, alt_type, &token_in, &token_out)?;
        }

        let mut pending: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::PendingOrders)
            .unwrap_or_else(|| Vec::new(&env));
        if pending.len() >= max_orders(&env) {
            return Err(AuctionError::BatchFull);
        }

        trader.require_auth();

        // Escrow input tokens immediately so the commitment is firm.
        SepTokenClient::new(&env, &token_in).transfer(
            &trader,
            &env.current_contract_address(),
            &amount_in,
        );

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextOrderId)
            .unwrap_or(0);

        let order = Order {
            id,
            trader: trader.clone(),
            pool,
            token_in,
            token_out,
            amount_in,
            min_out,
            submitted_at: env.ledger().timestamp(),
            deadline,
            pool_type,
            zero_for_one,
            sqrt_price_limit,
            alt_pool,
        };

        env.storage().persistent().set(&DataKey::Order(id), &order);
        bump_persistent(&env, &DataKey::Order(id));

        pending.push_back(id);
        env.storage()
            .instance()
            .set(&DataKey::PendingOrders, &pending);
        env.storage()
            .instance()
            .set(&DataKey::NextOrderId, &(id + 1));

        emit_versioned_event!(
            env,
            (Symbol::new(&env, "order_submitted"), trader),
            (id, amount_in)
        );

        Ok(id)
    }

    /// Cancel a pending order and refund escrowed tokens.
    ///
    /// Only the original trader may cancel their own order.
    pub fn cancel_order(env: Env, trader: Address, order_id: u64) -> Result<(), AuctionError> {
        extend_ttl(&env);
        trader.require_auth();

        let order: Order = env
            .storage()
            .persistent()
            .get(&DataKey::Order(order_id))
            .ok_or(AuctionError::OrderNotFound)?;

        if order.trader != trader {
            return Err(AuctionError::Unauthorized);
        }

        // Refund escrowed tokens.
        SepTokenClient::new(&env, &order.token_in).transfer(
            &env.current_contract_address(),
            &trader,
            &order.amount_in,
        );

        env.storage().persistent().remove(&DataKey::Order(order_id));

        let pending: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::PendingOrders)
            .unwrap_or_else(|| Vec::new(&env));
        let mut updated = Vec::<u64>::new(&env);
        for i in 0..pending.len() {
            let oid = pending.get(i).unwrap();
            if oid != order_id {
                updated.push_back(oid);
            }
        }
        env.storage()
            .instance()
            .set(&DataKey::PendingOrders, &updated);

        emit_versioned_event!(
            env,
            (Symbol::new(&env, "order_cancelled"), trader),
            (order_id,)
        );

        Ok(())
    }

    /// Settle the current batch atomically.
    ///
    /// Callable by anyone once the batch window has elapsed. Pending orders
    /// are processed sequentially within a single transaction — no external
    /// trade can be inserted between them, which structurally prevents
    /// sandwich attacks.
    ///
    /// Each order's swap is attempted in isolation (issue #473): if an order
    /// has become unfillable at settlement time (its `min_out` can no longer
    /// be met, its venue is paused, or in-range liquidity is insufficient),
    /// that single order is skipped — its escrow is refunded to the trader
    /// and it is dropped from the batch — instead of reverting the whole
    /// settlement and freezing every other trader's escrow.
    ///
    /// Token transfers (payout on success, refund on failure/expiry) are also
    /// isolated via `try_transfer` (issue #546): a single trader's inability
    /// to receive tokens — no trustline, insufficient limit, frozen/revoked
    /// SAC authorization, clawback-enabled asset — does not revert the entire
    /// batch. The failing order is logged as `order_failed` and settlement
    /// continues with the remaining orders.
    ///
    /// Returns the output amounts for each order that settled successfully,
    /// in submission order. Orders that failed and were refunded are omitted;
    /// callers should compare against `get_pending_orders` before and after
    /// to see which orders failed, and use `order_failed` events to react.
    pub fn settle_batch(env: Env) -> Result<Vec<i128>, AuctionError> {
        extend_ttl(&env);
        let opened_at: u64 = env
            .storage()
            .instance()
            .get(&DataKey::BatchOpenedAt)
            .unwrap_or(0);
        let window_secs: u64 = env
            .storage()
            .instance()
            .get(&DataKey::BatchWindowSecs)
            .unwrap_or(60);
        let now = env.ledger().timestamp();
        // Checked: a window loaded from legacy storage can be arbitrarily large.
        if now < opened_at.saturating_add(window_secs) {
            return Err(AuctionError::BatchWindowOpen);
        }

        let pending: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::PendingOrders)
            .unwrap_or_else(|| Vec::new(&env));
        if pending.is_empty() {
            return Err(AuctionError::NoOrders);
        }

        let auction_addr = env.current_contract_address();
        let order_limit = max_orders(&env);
        let process_count = if pending.len() > order_limit {
            order_limit
        } else {
            pending.len()
        };
        let mut results = Vec::<i128>::new(&env);

        for i in 0..process_count {
            let order_id = pending.get(i).unwrap();
            let order: Order = env
                .storage()
                .persistent()
                .get(&DataKey::Order(order_id))
                .unwrap();

            // Honor the trader's own deadline. If it has already passed by
            // settlement time, expire the order and refund its escrow rather
            // than silently substituting a freshly-computed deadline.
            if now > order.deadline {
                if Self::pay_or_claim(
                    &env,
                    order_id,
                    &order.trader,
                    &order.token_in,
                    order.amount_in,
                ) {
                    results.push_back(0);
                }
                emit_versioned_event!(
                    env,
                    (Symbol::new(&env, "order_expired"), order.trader.clone()),
                    (order_id,)
                );
                env.storage().persistent().remove(&DataKey::Order(order_id));
                continue;
            }

            // Re-validate the venue at settlement time too: an order's pool
            // may have been admin-removed from the allowlist (or the factory
            // reconfigured) since submission. Refund rather than executing
            // against a venue that is no longer trusted (issue #700).
            if !Self::venue_is_registered(&env, &order.pool, order.pool_type) {
                Self::pay_or_claim(
                    &env,
                    order_id,
                    &order.trader,
                    &order.token_in,
                    order.amount_in,
                );
                emit_versioned_event!(
                    env,
                    (
                        Symbol::new(&env, "order_venue_removed"),
                        order.trader.clone(),
                    ),
                    (order_id,)
                );
                env.storage().persistent().remove(&DataKey::Order(order_id));
                continue;
            }

            // Execute the swap on behalf of the batch auction contract, routing
            // to whichever supported venue gives the best output. Authorization
            // for the token pull (auction → pool) is automatically satisfied
            // because the batch_auction is the invoking contract. A failure
            // here (min_out no longer met, venue paused, insufficient in-range
            // liquidity) is isolated to this order rather than reverting the
            // whole batch (issue #473). The deadline passed in is the trader's
            // own submitted bound, not a freshly-computed one.
            match Self::execute_op(&env, &order, &auction_addr, order.deadline) {
                Ok(amount_out) => {
                    // Forward output tokens to the original trader.
                    if Self::pay_or_claim(
                        &env,
                        order_id,
                        &order.trader,
                        &order.token_out,
                        amount_out,
                    ) {
                        results.push_back(amount_out);
                        emit_versioned_event!(
                            env,
                            (Symbol::new(&env, "order_settled"), order.trader.clone()),
                            (order_id, amount_out)
                        );
                    } else {
                        // Swap succeeded but payout to the trader failed (no
                        // trustline, frozen/revoked auth, clawback, etc.). The
                        // amount is now claimable via `claim_refund` instead
                        // of being stranded; the order is still dropped so
                        // the batch can continue.
                        emit_versioned_event!(
                            env,
                            (Symbol::new(&env, "order_failed"), order.trader.clone()),
                            (order_id,)
                        );
                    }
                }
                Err(()) => {
                    // Unfillable order: the failed swap attempt was rolled back
                    // by the runtime, so the full escrow is still held by the
                    // auction contract. Refund it and drop the order instead of
                    // letting it block every other trader's settlement.
                    Self::pay_or_claim(
                        &env,
                        order_id,
                        &order.trader,
                        &order.token_in,
                        order.amount_in,
                    );
                    emit_versioned_event!(
                        env,
                        (Symbol::new(&env, "order_failed"), order.trader.clone()),
                        (order_id,)
                    );
                }
            }
            env.storage().persistent().remove(&DataKey::Order(order_id));
        }

        let mut remaining = Vec::<u64>::new(&env);
        for i in process_count..pending.len() {
            remaining.push_back(pending.get(i).unwrap());
        }

        env.storage()
            .instance()
            .set(&DataKey::PendingOrders, &remaining);
        env.storage().instance().set(&DataKey::BatchOpenedAt, &now);

        emit_versioned_event!(env, (symbol_short!("settled"),), (process_count,));

        Ok(results)
    }

    /// Attempt to pay `amount` of `token` to `trader`. On success, returns
    /// `true`. On failure (no trustline, frozen/revoked auth, clawback,
    /// etc.), records the amount as claimable via `claim_refund` instead of
    /// stranding it in the contract forever, emits `order_refund_failed`, and
    /// returns `false` (issue #700).
    fn pay_or_claim(
        env: &Env,
        order_id: u64,
        trader: &Address,
        token: &Address,
        amount: i128,
    ) -> bool {
        let auction_addr = env.current_contract_address();
        let ok = SepTokenClient::new(env, token)
            .try_transfer(&auction_addr, trader, &amount)
            .ok()
            .and_then(|r| r.ok())
            .is_some();
        if !ok {
            env.storage().persistent().set(
                &DataKey::Claimable(order_id),
                &(trader.clone(), token.clone(), amount),
            );
            bump_persistent(env, &DataKey::Claimable(order_id));
            emit_versioned_event!(
                env,
                (Symbol::new(env, "order_refund_failed"), trader.clone()),
                (order_id,)
            );
        }
        ok
    }

    /// Refund a single order's escrow immediately once its deadline has
    /// passed, without waiting for the batch window to elapse (issue #700).
    /// Permissionless — anyone (e.g. a keeper) may call this on the trader's
    /// behalf. Idempotent: once refunded, the order no longer exists, so a
    /// second call returns `OrderNotFound` rather than double-refunding.
    pub fn expire_order(env: Env, order_id: u64) -> Result<(), AuctionError> {
        extend_ttl(&env);
        let order: Order = env
            .storage()
            .persistent()
            .get(&DataKey::Order(order_id))
            .ok_or(AuctionError::OrderNotFound)?;
        let now = env.ledger().timestamp();
        if now <= order.deadline {
            return Err(AuctionError::OrderNotExpired);
        }
        Self::pay_or_claim(
            &env,
            order_id,
            &order.trader,
            &order.token_in,
            order.amount_in,
        );
        env.storage().persistent().remove(&DataKey::Order(order_id));
        let pending: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::PendingOrders)
            .unwrap_or_else(|| Vec::new(&env));
        let mut updated = Vec::<u64>::new(&env);
        for oid in pending.iter() {
            if oid != order_id {
                updated.push_back(oid);
            }
        }
        env.storage()
            .instance()
            .set(&DataKey::PendingOrders, &updated);
        emit_versioned_event!(
            env,
            (Symbol::new(&env, "order_expired"), order.trader),
            (order_id,)
        );
        Ok(())
    }

    /// Return the IDs of pending orders whose deadline has already passed,
    /// for keepers to drive `expire_order` (issue #700).
    pub fn get_expired_orders(env: Env) -> Vec<u64> {
        extend_ttl(&env);
        let now = env.ledger().timestamp();
        let pending: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::PendingOrders)
            .unwrap_or_else(|| Vec::new(&env));
        let mut expired = Vec::new(&env);
        for oid in pending.iter() {
            if let Some(order) = env
                .storage()
                .persistent()
                .get::<_, Order>(&DataKey::Order(oid))
            {
                if now > order.deadline {
                    expired.push_back(oid);
                }
            }
        }
        expired
    }

    /// Claim a stranded refund/payout left behind by a failed transfer during
    /// `settle_batch`/`expire_order` (issue #700). `trader` must be the same
    /// address the claimable amount was recorded for.
    pub fn claim_refund(env: Env, trader: Address, order_id: u64) -> Result<i128, AuctionError> {
        extend_ttl(&env);
        trader.require_auth();
        let (owner, token, amount): (Address, Address, i128) = env
            .storage()
            .persistent()
            .get(&DataKey::Claimable(order_id))
            .ok_or(AuctionError::NothingToClaim)?;
        if owner != trader {
            return Err(AuctionError::Unauthorized);
        }
        env.storage()
            .persistent()
            .remove(&DataKey::Claimable(order_id));
        SepTokenClient::new(&env, &token).transfer(
            &env.current_contract_address(),
            &trader,
            &amount,
        );
        emit_versioned_event!(
            env,
            (Symbol::new(&env, "refund_claimed"), trader),
            (order_id, amount)
        );
        Ok(amount)
    }

    /// Quote the output an order would receive on each candidate venue and
    /// return the best `(amount_out, pool, pool_type)` triple (issue #351).
    ///
    /// Read-only: callers can preview the venue settlement would choose. The
    /// quote walks the same pool math used at execution, so the chosen venue
    /// matches `settle_batch`'s routing for an unchanged pool state.
    pub fn quote_order(env: Env, order_id: u64) -> Result<(i128, Address, PoolType), AuctionError> {
        extend_ttl(&env);
        let order: Order = env
            .storage()
            .persistent()
            .get(&DataKey::Order(order_id))
            .ok_or(AuctionError::OrderNotFound)?;
        Ok(Self::best_venue(&env, &order))
    }

    /// Dispatch a single order's swap to the best available venue and return
    /// the realized output amount.
    ///
    /// Branches on [`PoolType`]: `Amm` settles through [`AmmPoolClient`], `Cl`
    /// through [`ConcentratedLiquidityClient`]. When the chosen venue consumes
    /// less than `amount_in` (a concentrated-liquidity pool can fill partially
    /// once it exhausts in-range liquidity), the unspent escrow is refunded to
    /// the trader so no funds are stranded in the auction contract.
    ///
    /// Calls the venue through its `try_` entry point (issue #473): a pool
    /// error (slippage/`min_out` not met, paused, insufficient liquidity) is
    /// returned as `Err(())` instead of escalating to a host panic, so the
    /// caller can isolate the failure to this one order.
    ///
    /// The unspent-refund transfer is also wrapped with `try_transfer`
    /// (issue #546) so a trustline/auth failure on the refund leg cannot
    /// revert the caller.
    fn execute_op(env: &Env, order: &Order, sender: &Address, deadline: u64) -> Result<i128, ()> {
        let (_, venue, venue_type) = Self::best_venue(env, order);

        let spent_before = SepTokenClient::new(env, &order.token_in).balance(sender);
        let swap_result = match venue_type {
            PoolType::Amm => AmmPoolClient::new(env, &venue)
                .try_swap(
                    sender,
                    &order.token_in,
                    &order.amount_in,
                    &order.min_out,
                    &deadline,
                )
                .ok()
                .and_then(|r| r.ok()),
            PoolType::Cl => ConcentratedLiquidityClient::new(env, &venue)
                .try_swap(
                    sender,
                    &order.zero_for_one,
                    &order.amount_in,
                    &order.sqrt_price_limit,
                    &order.min_out,
                    &deadline,
                )
                .ok()
                .and_then(|r| r.ok()),
        };
        let amount_out = match swap_result {
            Some(amount_out) => amount_out,
            None => return Err(()),
        };
        let spent_after = SepTokenClient::new(env, &order.token_in).balance(sender);

        // Refund any input the venue did not consume (partial fill).
        let spent = spent_before - spent_after;
        let unspent = order.amount_in - spent;
        if unspent > 0 {
            SepTokenClient::new(env, &order.token_in)
                .try_transfer(sender, &order.trader, &unspent)
                .ok()
                .and_then(|r| r.ok());
        }

        Ok(amount_out)
    }

    /// Pick the venue that quotes the most output for `order`.
    ///
    /// Always considers the primary `(pool, pool_type)`. If `alt_pool` is set it
    /// is treated as a venue of the opposite type and compared; the higher quote
    /// wins, with the primary kept on ties or when the alternate cannot be
    /// quoted. Quotes are best-effort: a venue that fails to quote is simply not
    /// selected, so a stale or unrelated alternate can never block settlement.
    fn best_venue(env: &Env, order: &Order) -> (i128, Address, PoolType) {
        let primary_q = Self::try_quote(env, &order.pool, order.pool_type, order).unwrap_or(0);

        if let Some(alt) = order.alt_pool.clone() {
            let alt_type = match order.pool_type {
                PoolType::Amm => PoolType::Cl,
                PoolType::Cl => PoolType::Amm,
            };
            // Re-check the alternate venue at routing time (pair *and*
            // legitimacy) so a malformed order written before validation was
            // added, or a venue removed since submission, can never be routed
            // into the wrong output asset or an untrusted contract.
            if Self::check_venue(env, &alt, alt_type, &order.token_in, &order.token_out).is_ok() {
                if let Some(alt_q) = Self::try_quote(env, &alt, alt_type, order) {
                    if alt_q > primary_q {
                        return (alt_q, alt, alt_type);
                    }
                }
            }
        }
        (primary_q, order.pool.clone(), order.pool_type)
    }

    /// Return whether `pool` trades exactly the unordered `(token_in, token_out)`
    /// pair for the requested venue type.
    fn pool_matches_pair(
        env: &Env,
        pool: &Address,
        pool_type: PoolType,
        token_in: &Address,
        token_out: &Address,
    ) -> bool {
        let (pool_token_a, pool_token_b) = match pool_type {
            PoolType::Amm => {
                let info = AmmPoolClient::new(env, pool).get_info();
                (info.token_a, info.token_b)
            }
            PoolType::Cl => ConcentratedLiquidityClient::new(env, pool).get_tokens(),
        };
        (token_in == &pool_token_a && token_out == &pool_token_b)
            || (token_in == &pool_token_b && token_out == &pool_token_a)
    }

    /// Validate `pool` as a legitimate settlement venue trading exactly the
    /// unordered `(token_in, token_out)` pair (issue #700).
    ///
    /// The pair check is always performed first, against the venue's own
    /// reported tokens — cheap, and it gives a precise `InvalidPoolTokenPair`
    /// for a real, legitimate pool simply named with the wrong pair. It is
    /// never, by itself, a security boundary: an attacker's contract can
    /// self-report any pair it likes, so legitimacy is established
    /// separately below and failing *that* is reported as `UnknownVenue`.
    ///
    /// A venue is legitimate if either:
    /// - it is on the admin allowlist (`add_venue`); or
    /// - the configured factory attests to it: for an AMM venue, the
    ///   factory's own `(token_a, token_b)` record for `pool` must match;
    ///   for a CL venue, the factory must resolve `(token_in, token_out,
    ///   pool's own fee_bps)` back to exactly `pool`.
    ///
    /// Either path anchors legitimacy in a registry this protocol controls,
    /// rather than trusting metadata self-reported by an arbitrary address.
    fn check_venue(
        env: &Env,
        pool: &Address,
        pool_type: PoolType,
        token_in: &Address,
        token_out: &Address,
    ) -> Result<(), AuctionError> {
        if !Self::pool_matches_pair(env, pool, pool_type, token_in, token_out) {
            return Err(AuctionError::InvalidPoolTokenPair);
        }
        if Self::is_venue_allowed(env.clone(), pool.clone()) {
            return Ok(());
        }
        let factory: Option<Address> = env.storage().instance().get(&DataKey::Factory);
        let Some(factory) = factory else {
            return Err(AuctionError::UnknownVenue);
        };
        let factory_client = FactoryClient::new(env, &factory);
        let attested = match pool_type {
            PoolType::Amm => matches!(
                factory_client
                    .try_get_pool_tokens(pool)
                    .ok()
                    .and_then(|r| r.ok())
                    .flatten(),
                Some((a, b)) if (token_in == &a && token_out == &b) || (token_in == &b && token_out == &a)
            ),
            PoolType::Cl => {
                let fee_bps = match ConcentratedLiquidityClient::new(env, pool).try_fee_bps() {
                    Ok(Ok(v)) => v,
                    _ => return Err(AuctionError::UnknownVenue),
                };
                matches!(
                    factory_client
                        .try_get_cl_pool(token_in, token_out, &fee_bps)
                        .ok()
                        .and_then(|r| r.ok())
                        .flatten(),
                    Some(resolved) if &resolved == pool
                )
            }
        };
        if attested {
            Ok(())
        } else {
            Err(AuctionError::UnknownVenue)
        }
    }

    /// Return whether `pool` is still a registered venue — on the admin
    /// allowlist, or attested to by the factory for *its own* reported
    /// tokens (issue #700).
    ///
    /// Unlike `check_venue`, this does not check `pool` against any specific
    /// `(token_in, token_out)` pair: it is used only to re-validate that a
    /// venue named in an already-submitted order hasn't since been removed
    /// (e.g. via `remove_venue`), which is orthogonal to whatever tokens that
    /// order recorded at submission time.
    fn venue_is_registered(env: &Env, pool: &Address, pool_type: PoolType) -> bool {
        if Self::is_venue_allowed(env.clone(), pool.clone()) {
            return true;
        }
        let factory: Option<Address> = env.storage().instance().get(&DataKey::Factory);
        let Some(factory) = factory else {
            return false;
        };
        let factory_client = FactoryClient::new(env, &factory);
        match pool_type {
            PoolType::Amm => factory_client
                .try_get_pool_tokens(pool)
                .ok()
                .and_then(|r| r.ok())
                .flatten()
                .is_some(),
            PoolType::Cl => {
                let cl_client = ConcentratedLiquidityClient::new(env, pool);
                let fee_bps = match cl_client.try_fee_bps() {
                    Ok(Ok(v)) => v,
                    _ => return false,
                };
                let (a, b) = match cl_client.try_get_tokens() {
                    Ok(Ok(v)) => v,
                    _ => return false,
                };
                matches!(
                    factory_client
                        .try_get_cl_pool(&a, &b, &fee_bps)
                        .ok()
                        .and_then(|r| r.ok())
                        .flatten(),
                    Some(resolved) if &resolved == pool
                )
            }
        }
    }

    /// Read-only output quote for `order` on `pool` interpreted as `pool_type`.
    /// Returns `None` if the venue rejects the quote (e.g. wrong token pair).
    fn try_quote(env: &Env, pool: &Address, pool_type: PoolType, order: &Order) -> Option<i128> {
        match pool_type {
            PoolType::Amm => AmmPoolClient::new(env, pool)
                .try_get_amount_out(&order.token_in, &order.amount_in)
                .ok()?
                .ok(),
            PoolType::Cl => Some(
                ConcentratedLiquidityClient::new(env, pool)
                    .try_estimate_price_impact(
                        &order.zero_for_one,
                        &order.amount_in,
                        &order.sqrt_price_limit,
                    )
                    .ok()?
                    .ok()?
                    .amount_out,
            ),
        }
    }

    /// Return all pending orders in the current batch window.
    pub fn get_pending_orders(env: Env) -> Vec<Order> {
        extend_ttl(&env);
        let pending: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::PendingOrders)
            .unwrap_or_else(|| Vec::new(&env));
        let mut orders = Vec::<Order>::new(&env);
        for i in 0..pending.len() {
            let id = pending.get(i).unwrap();
            if let Some(order) = env.storage().persistent().get(&DataKey::Order(id)) {
                orders.push_back(order);
            }
        }
        orders
    }

    /// Return compact batch capacity and timing metadata.
    ///
    /// The tuple is `(pending_count, max_orders, batch_opened_at,
    /// batch_window_secs)`.
    pub fn get_batch_info(env: Env) -> (u32, u32, u64, u64) {
        extend_ttl(&env);
        let pending: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::PendingOrders)
            .unwrap_or_else(|| Vec::new(&env));
        let opened_at: u64 = env
            .storage()
            .instance()
            .get(&DataKey::BatchOpenedAt)
            .unwrap_or(0);
        let window_secs: u64 = env
            .storage()
            .instance()
            .get(&DataKey::BatchWindowSecs)
            .unwrap_or(60);

        (pending.len(), max_orders(&env), opened_at, window_secs)
    }

    /// Update the batch window duration. Admin-only.
    ///
    /// The window must lie within the configured bounds (default
    /// `MIN_BATCH_WINDOW_SECS`..=`MAX_BATCH_WINDOW_SECS`, see
    /// `set_window_bounds`); `0` and `u64::MAX` are rejected with
    /// `InvalidBatchWindow`, so a mis-set window can neither disable batching
    /// nor freeze settlement (issue #1119).
    pub fn set_batch_window(env: Env, batch_window_secs: u64) -> Result<(), AuctionError> {
        extend_ttl(&env);
        let admin: Address = read_admin(&env)?;
        admin.require_auth();
        validate_window(&env, batch_window_secs)?;
        env.storage()
            .instance()
            .set(&DataKey::BatchWindowSecs, &batch_window_secs);
        emit_versioned_event!(
            env,
            (Symbol::new(&env, "window_updated"),),
            (batch_window_secs,)
        );
        Ok(())
    }

    /// Set the `[min_secs, max_secs]` bounds enforced on the batch window.
    /// Admin-only (issue #1119).
    ///
    /// Requires `0 < min_secs <= max_secs <= WINDOW_BOUNDS_CEILING_SECS` and
    /// that the current window still lies inside the new bounds; otherwise
    /// `InvalidWindowBounds`.
    pub fn set_window_bounds(
        env: Env,
        admin: Address,
        min_secs: u64,
        max_secs: u64,
    ) -> Result<(), AuctionError> {
        extend_ttl(&env);
        let stored_admin = read_admin(&env)?;
        if stored_admin != admin {
            return Err(AuctionError::Unauthorized);
        }
        admin.require_auth();
        if min_secs == 0 || min_secs > max_secs || max_secs > WINDOW_BOUNDS_CEILING_SECS {
            return Err(AuctionError::InvalidWindowBounds);
        }
        let current: u64 = env
            .storage()
            .instance()
            .get(&DataKey::BatchWindowSecs)
            .unwrap_or(min_secs);
        if current < min_secs || current > max_secs {
            return Err(AuctionError::InvalidWindowBounds);
        }
        env.storage()
            .instance()
            .set(&DataKey::WindowBounds, &(min_secs, max_secs));
        emit_versioned_event!(
            env,
            (Symbol::new(&env, "window_bounds_updated"),),
            (min_secs, max_secs)
        );
        Ok(())
    }

    /// The `(min_secs, max_secs)` bounds currently enforced on the batch window.
    pub fn get_window_bounds(env: Env) -> (u64, u64) {
        extend_ttl(&env);
        window_bounds(&env)
    }

    /// Update the maximum number of orders accepted into a batch. Admin-only.
    ///
    /// `n` must be between 1 and `MAX_ORDERS_CEILING`, inclusive. The ceiling
    /// keeps settlement cost bounded even if governance/admin configuration is
    /// changed under production load.
    pub fn set_max_orders(env: Env, admin: Address, n: u32) -> Result<(), AuctionError> {
        extend_ttl(&env);
        let stored_admin: Address = read_admin(&env)?;
        if stored_admin != admin {
            return Err(AuctionError::Unauthorized);
        }
        admin.require_auth();
        if n == 0 || n > MAX_ORDERS_CEILING {
            return Err(AuctionError::InvalidMaxOrders);
        }

        env.storage().instance().set(&DataKey::MaxOrders, &n);
        emit_versioned_event!(env, (Symbol::new(&env, "max_orders_updated"),), (n,));
        Ok(())
    }

    /// Configure the factory that attests to protocol-deployed pools
    /// (issue #700). Admin-only; may be updated to rotate the factory.
    pub fn set_factory(env: Env, admin: Address, factory: Address) -> Result<(), AuctionError> {
        extend_ttl(&env);
        let stored_admin: Address = read_admin(&env)?;
        if stored_admin != admin {
            return Err(AuctionError::Unauthorized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Factory, &factory);
        emit_versioned_event!(env, (Symbol::new(&env, "factory_updated"),), (factory,));
        Ok(())
    }

    /// Admin-managed allowlist entry for a venue the factory doesn't (or
    /// can't yet) attest to (issue #700).
    pub fn add_venue(
        env: Env,
        admin: Address,
        pool: Address,
        pool_type: PoolType,
    ) -> Result<(), AuctionError> {
        extend_ttl(&env);
        let stored_admin: Address = read_admin(&env)?;
        if stored_admin != admin {
            return Err(AuctionError::Unauthorized);
        }
        admin.require_auth();
        if !env.storage().instance().has(&DataKey::Venue(pool.clone())) {
            let mut list: Vec<Address> = env
                .storage()
                .instance()
                .get(&DataKey::VenueList)
                .unwrap_or_else(|| Vec::new(&env));
            if list.len() >= MAX_VENUES {
                return Err(AuctionError::TooManyVenues);
            }
            list.push_back(pool.clone());
            env.storage().instance().set(&DataKey::VenueList, &list);
        }
        env.storage()
            .instance()
            .set(&DataKey::Venue(pool.clone()), &pool_type);
        emit_versioned_event!(env, (Symbol::new(&env, "venue_added"), pool), (pool_type,));
        Ok(())
    }

    /// Remove a venue from the admin allowlist (issue #700). A venue removed
    /// after an order was submitted against it causes that order to be
    /// refunded at settlement rather than executed (see `settle_batch`).
    pub fn remove_venue(env: Env, admin: Address, pool: Address) -> Result<(), AuctionError> {
        extend_ttl(&env);
        let stored_admin: Address = read_admin(&env)?;
        if stored_admin != admin {
            return Err(AuctionError::Unauthorized);
        }
        admin.require_auth();
        env.storage()
            .instance()
            .remove(&DataKey::Venue(pool.clone()));
        let list: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::VenueList)
            .unwrap_or_else(|| Vec::new(&env));
        let mut updated = Vec::<Address>::new(&env);
        for addr in list.iter() {
            if addr != pool {
                updated.push_back(addr);
            }
        }
        env.storage().instance().set(&DataKey::VenueList, &updated);
        emit_versioned_event!(env, (Symbol::new(&env, "venue_removed"), pool), ());
        Ok(())
    }

    /// Return whether `pool` is on the admin allowlist.
    pub fn is_venue_allowed(env: Env, pool: Address) -> bool {
        extend_ttl(&env);
        env.storage().instance().has(&DataKey::Venue(pool))
    }

    /// Return admin-allowlisted venues, `(pool, pool_type)`, paginated.
    ///
    /// Defensive (issue #1119): the offset and limit may be anything, including
    /// `u32::MAX`; the page is clamped to the list, and an index entry without a
    /// `Venue(pool)` record is skipped rather than trapping.
    pub fn list_venues(env: Env, offset: u32, limit: u32) -> Vec<(Address, PoolType)> {
        extend_ttl(&env);
        let list: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::VenueList)
            .unwrap_or_else(|| Vec::new(&env));
        let mut out = Vec::new(&env);
        let end = offset.saturating_add(limit).min(list.len());
        let mut i = offset;
        while i < end {
            if let Some(pool) = list.get(i) {
                let pool_type: Option<PoolType> =
                    env.storage().instance().get(&DataKey::Venue(pool.clone()));
                if let Some(pool_type) = pool_type {
                    out.push_back((pool, pool_type));
                }
            }
            i += 1;
        }
        out
    }

    /// Nominate a new admin. The nominee must call `accept_admin` to complete
    /// the transfer (two-step rotation, matching `pol_vesting` / AMM).
    ///
    /// Prevents a mistyped address from permanently locking `set_batch_window`
    /// and `set_max_orders` (issue #553).
    pub fn propose_admin(
        env: Env,
        current_admin: Address,
        new_admin: Address,
    ) -> Result<(), AuctionError> {
        extend_ttl(&env);
        let stored: Address = read_admin(&env)?;
        if current_admin != stored {
            return Err(AuctionError::Unauthorized);
        }
        current_admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Some(new_admin.clone()));
        emit_versioned_event!(
            env,
            (Symbol::new(&env, "admin_nominated"),),
            (current_admin, new_admin)
        );
        Ok(())
    }

    /// Accept the pending admin nomination. Caller becomes the new admin.
    pub fn accept_admin(env: Env, new_admin: Address) -> Result<(), AuctionError> {
        extend_ttl(&env);
        read_admin(&env)?;
        let pending: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None);
        let nominee = pending.ok_or(AuctionError::NoPendingAdmin)?;
        if new_admin != nominee {
            return Err(AuctionError::WrongAdmin);
        }
        new_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Option::<Address>::None);
        emit_versioned_event!(env, (Symbol::new(&env, "admin_changed"),), (new_admin,));
        Ok(())
    }

    /// Return the active admin address, or `NotInitialized` before `initialize`.
    pub fn get_admin(env: Env) -> Result<Address, AuctionError> {
        extend_ttl(&env);
        read_admin(&env)
    }

    /// Return the pending admin nominee, or `None` if no transfer is in progress.
    pub fn get_pending_admin(env: Env) -> Option<Address> {
        extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None)
    }

    /// Return the stranded `(trader, token, amount)` claimable for `order_id`,
    /// or `None` if there is nothing to claim (issue #700).
    pub fn get_claimable(env: Env, order_id: u64) -> Option<(Address, Address, i128)> {
        extend_ttl(&env);
        let key = DataKey::Claimable(order_id);
        let value: Option<(Address, Address, i128)> = env.storage().persistent().get(&key);
        if value.is_some() {
            bump_persistent(&env, &key);
        }
        value
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use amm::{AmmPool, AmmPoolClient};
    use concentrated_liquidity::{ConcentratedLiquidity, ConcentratedLiquidityClient};
    use factory::{Factory, FactoryClient};
    use pool_interfaces::{AmmError, PoolInfo};
    use soroban_sdk::{
        testutils::{Address as _, Events, Ledger},
        token::{StellarAssetClient, TokenClient as StellarTokenClient},
        Env, String, TryFromVal,
    };
    use token::{LpToken, LpTokenClient};

    /// Test-only hostile settlement venue (issue #700): reports whatever
    /// token pair it's initialized with via `get_info` (self-reported, like
    /// any real pool) and, if ever actually reached, drains its caller's
    /// input while fabricating a large output. Used to prove the venue
    /// registry rejects it at submission — before it can ever be dispatched
    /// to from `settle_batch`.
    #[contract]
    struct HostileVenue;

    #[contractimpl]
    impl HostileVenue {
        pub fn initialize(env: Env, token_a: Address, token_b: Address) {
            env.storage()
                .instance()
                .set(&symbol_short!("toks"), &(token_a, token_b));
        }

        pub fn get_info(env: Env) -> PoolInfo {
            let (token_a, token_b): (Address, Address) = env
                .storage()
                .instance()
                .get(&symbol_short!("toks"))
                .unwrap();
            PoolInfo {
                token_a: token_a.clone(),
                token_b,
                reserve_a: 0,
                reserve_b: 0,
                total_shares: 0,
                fee_bps: 0,
                flash_loan_fee_bps: 0,
                admin: token_a.clone(),
                fee_recipient: token_a,
                protocol_fee_bps: 0,
                lp_rebate_bps: 0,
            }
        }

        pub fn swap(
            env: Env,
            trader: Address,
            token_in: Address,
            amount_in: i128,
            _min_out: i128,
            _deadline: u64,
        ) -> Result<i128, AmmError> {
            // If ever reached, this is exactly the attack the venue registry
            // must block: drain the caller's input and fabricate a payout.
            SepTokenClient::new(&env, &token_in).transfer(
                &trader,
                &env.current_contract_address(),
                &amount_in,
            );
            Ok(amount_in.saturating_mul(1_000))
        }
    }

    /// A deadline safely within `MAX_ORDER_LIFETIME_SECS` of `env`'s current
    /// ledger time, for tests that don't care about deadline expiry and
    /// previously used `u64::MAX` before the ceiling existed (issue #700).
    fn far_future_deadline(env: &Env) -> u64 {
        env.ledger().timestamp() + MAX_ORDER_LIFETIME_SECS - 1
    }

    /// Deploy a concentrated-liquidity pool over `(token_a, token_b)`, seed a
    /// wide in-range position, and return the pool address. The pool starts at
    /// tick 0 with tick spacing 10 and a 30 bps fee.
    fn deploy_cl_pool(env: &Env, admin: &Address, token_a: &Address, token_b: &Address) -> Address {
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let cl = ConcentratedLiquidityClient::new(env, &cl_addr);
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
        cl_addr
    }

    fn deploy_pool(env: &Env, token_a: &Address, token_b: &Address) -> Address {
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        LpTokenClient::new(env, &lp_addr).initialize(
            &amm_addr,
            &String::from_str(env, "LP"),
            &String::from_str(env, "LP"),
            &7u32,
        );
        AmmPoolClient::new(env, &amm_addr).initialize(
            &amm_addr, token_a, token_b, &lp_addr, &30_i128, &amm_addr, &0_i128,
        );
        amm_addr
    }

    fn setup(env: &Env) -> (Address, Address, Address, Address) {
        let admin = Address::generate(env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let pool = deploy_pool(env, &ta, &tb);

        let lp = Address::generate(env);
        StellarAssetClient::new(env, &ta).mint(&lp, &2_000_000_i128);
        StellarAssetClient::new(env, &tb).mint(&lp, &2_000_000_i128);
        AmmPoolClient::new(env, &pool).add_liquidity(
            &lp,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &far_future_deadline(env),
        );
        (ta, tb, pool, admin)
    }

    #[test]
    fn test_submit_and_settle() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        BatchAuctionClient::new(&env, &auction_addr).initialize(&admin, &30_u64);
        BatchAuctionClient::new(&env, &auction_addr).add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        BatchAuctionClient::new(&env, &auction_addr).submit_order(
            &trader,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        // Advance past the batch window.
        env.ledger().set_timestamp(1031);

        let results = BatchAuctionClient::new(&env, &auction_addr).settle_batch();

        assert_eq!(results.len(), 1);
        assert!(results.get(0).unwrap() > 0);

        // Trader received token_b.
        let tb_balance = StellarTokenClient::new(&env, &tb).balance(&trader);
        assert!(tb_balance > 0);
    }

    #[test]
    fn test_submit_order_rejects_mismatched_pool_tokens() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, _tb, pool, admin) = setup(&env);

        // A token that is not part of the pool's pair.
        let foreign_admin = Address::generate(&env);
        let foreign = env
            .register_stellar_asset_contract_v2(foreign_admin)
            .address();

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        // token_out is not the pool's other token → rejected up front.
        let result = client.try_submit_order(
            &trader,
            &pool,
            &ta,
            &foreign,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );
        assert_eq!(result, Err(Ok(AuctionError::InvalidPoolTokenPair)));

        // The order is rejected before any escrow, so the trader keeps its funds
        // and no order is recorded in the batch.
        assert_eq!(
            StellarTokenClient::new(&env, &ta).balance(&trader),
            100_000_i128
        );
        assert_eq!(client.get_pending_orders().len(), 0);
    }

    #[test]
    fn test_cancel_order_refunds_tokens() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        BatchAuctionClient::new(&env, &auction_addr).initialize(&admin, &30_u64);
        BatchAuctionClient::new(&env, &auction_addr).add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        let order_id = BatchAuctionClient::new(&env, &auction_addr).submit_order(
            &trader,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        // Tokens were escrowed — trader's balance decreased.
        let balance_after_submit = StellarTokenClient::new(&env, &ta).balance(&trader);
        assert_eq!(balance_after_submit, 90_000_i128);

        BatchAuctionClient::new(&env, &auction_addr).cancel_order(&trader, &order_id);

        // Tokens returned after cancel.
        let balance_after_cancel = StellarTokenClient::new(&env, &ta).balance(&trader);
        assert_eq!(balance_after_cancel, 100_000_i128);

        let orders = BatchAuctionClient::new(&env, &auction_addr).get_pending_orders();
        assert_eq!(orders.len(), 0);
    }

    #[test]
    fn test_settle_before_window_reverts() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        BatchAuctionClient::new(&env, &auction_addr).initialize(&admin, &30_u64);
        BatchAuctionClient::new(&env, &auction_addr).add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);
        BatchAuctionClient::new(&env, &auction_addr).submit_order(
            &trader,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        // Window has not elapsed — should return BatchWindowOpen error.
        let result = BatchAuctionClient::new(&env, &auction_addr).try_settle_batch();
        assert!(result.is_err());
    }

    #[test]
    fn test_settle_batch_honors_order_deadline_not_freshly_computed_one() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        // Trader wants this order to expire well before the batch settles.
        client.submit_order(&trader, &pool, &ta, &tb, &10_000_i128, &0_i128, &1_010_u64);

        // Tokens are escrowed on submission.
        assert_eq!(
            StellarTokenClient::new(&env, &ta).balance(&trader),
            90_000_i128
        );

        // Advance past both the batch window and the order's own deadline.
        env.ledger().set_timestamp(1031);

        let results = client.settle_batch();
        assert_eq!(results.len(), 1);
        // The order was expired, not executed against a freshly-computed
        // deadline, so no output was produced.
        assert_eq!(results.get(0).unwrap(), 0);

        // Escrow was refunded in full rather than swapped.
        assert_eq!(
            StellarTokenClient::new(&env, &ta).balance(&trader),
            100_000_i128
        );
        assert_eq!(StellarTokenClient::new(&env, &tb).balance(&trader), 0);
        assert_eq!(client.get_pending_orders().len(), 0);
    }

    #[test]
    fn test_settle_batch_still_executes_orders_within_their_own_deadline() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        // Deadline is after settlement time, so the order must still execute.
        client.submit_order(&trader, &pool, &ta, &tb, &10_000_i128, &0_i128, &2_000_u64);

        env.ledger().set_timestamp(1031);

        let results = client.settle_batch();
        assert_eq!(results.len(), 1);
        assert!(results.get(0).unwrap() > 0);
        assert!(StellarTokenClient::new(&env, &tb).balance(&trader) > 0);
    }

    #[test]
    fn test_multiple_traders_in_same_batch() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        BatchAuctionClient::new(&env, &auction_addr).initialize(&admin, &60_u64);
        BatchAuctionClient::new(&env, &auction_addr).add_venue(&admin, &pool, &PoolType::Amm);

        let trader1 = Address::generate(&env);
        let trader2 = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader1, &50_000_i128);
        StellarAssetClient::new(&env, &ta).mint(&trader2, &50_000_i128);

        BatchAuctionClient::new(&env, &auction_addr).submit_order(
            &trader1,
            &pool,
            &ta,
            &tb,
            &5_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );
        BatchAuctionClient::new(&env, &auction_addr).submit_order(
            &trader2,
            &pool,
            &ta,
            &tb,
            &5_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        env.ledger().set_timestamp(1061);

        let results = BatchAuctionClient::new(&env, &auction_addr).settle_batch();

        assert_eq!(results.len(), 2);
        assert!(results.get(0).unwrap() > 0);
        assert!(results.get(1).unwrap() > 0);

        // Both traders received token_b.
        assert!(StellarTokenClient::new(&env, &tb).balance(&trader1) > 0);
        assert!(StellarTokenClient::new(&env, &tb).balance(&trader2) > 0);
    }

    // ── Issue #473: an unfillable order must not block the rest of the batch ──

    #[test]
    fn test_unfillable_order_is_refunded_and_does_not_block_batch() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let good_trader = Address::generate(&env);
        let bad_trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&good_trader, &10_000_i128);
        StellarAssetClient::new(&env, &ta).mint(&bad_trader, &10_000_i128);

        // An impossibly high min_out: this order can never clear at
        // settlement, no matter how the pool moves.
        client.submit_order(
            &bad_trader,
            &pool,
            &ta,
            &tb,
            &5_000_i128,
            &1_000_000_000_i128,
            &far_future_deadline(&env),
        );
        client.submit_order(
            &good_trader,
            &pool,
            &ta,
            &tb,
            &5_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        env.ledger().set_timestamp(1031);

        // Previously this reverted the whole batch, locking both escrows.
        let results = client.settle_batch();

        // Only the fillable order settled.
        assert_eq!(results.len(), 1);
        assert!(results.get(0).unwrap() > 0);

        // The good trader received token_b.
        assert!(StellarTokenClient::new(&env, &tb).balance(&good_trader) > 0);

        // The unfillable order's escrow was refunded in full, not stranded.
        assert_eq!(
            StellarTokenClient::new(&env, &ta).balance(&bad_trader),
            10_000_i128
        );
        assert_eq!(StellarTokenClient::new(&env, &tb).balance(&bad_trader), 0);

        // Both orders are cleared from the batch — the bad one is not left
        // pending to jam every future settlement attempt.
        assert_eq!(client.get_pending_orders().len(), 0);
    }

    #[test]
    fn test_submit_beyond_cap_returns_batch_full() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &60_u64);
        client.set_max_orders(&admin, &2_u32);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &10_000_i128);

        client.submit_order(
            &trader,
            &pool,
            &ta,
            &tb,
            &1_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );
        client.submit_order(
            &trader,
            &pool,
            &ta,
            &tb,
            &1_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        let result = client.try_submit_order(
            &trader,
            &pool,
            &ta,
            &tb,
            &1_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );
        assert_eq!(result, Err(Ok(AuctionError::BatchFull)));

        let (pending_count, max_orders, opened_at, window_secs) = client.get_batch_info();
        assert_eq!(pending_count, 2);
        assert_eq!(max_orders, 2);
        assert_eq!(opened_at, 1000);
        assert_eq!(window_secs, 60);

        let trader_balance = StellarTokenClient::new(&env, &ta).balance(&trader);
        assert_eq!(trader_balance, 8_000_i128);
    }

    #[test]
    fn test_settlement_with_exactly_max_orders_succeeds() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.set_max_orders(&admin, &3_u32);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &10_000_i128);

        for _ in 0..3 {
            client.submit_order(
                &trader,
                &pool,
                &ta,
                &tb,
                &1_000_i128,
                &0_i128,
                &far_future_deadline(&env),
            );
        }

        env.ledger().set_timestamp(1031);

        let results = client.settle_batch();
        assert_eq!(results.len(), 3);
        for i in 0..results.len() {
            assert!(results.get(i).unwrap() > 0);
        }

        let (pending_count, max_orders, opened_at, window_secs) = client.get_batch_info();
        assert_eq!(pending_count, 0);
        assert_eq!(max_orders, 3);
        assert_eq!(opened_at, 1031);
        assert_eq!(window_secs, 30);
        assert_eq!(client.get_pending_orders().len(), 0);
        assert!(StellarTokenClient::new(&env, &tb).balance(&trader) > 0);
    }

    // ── Issue #351: concentrated-liquidity settlement venue ────────────────────

    #[test]
    fn test_submit_and_settle_cl_order() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let admin = Address::generate(&env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_pool = deploy_cl_pool(&env, &admin, &ta, &tb);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.add_venue(&admin, &cl_pool, &PoolType::Cl);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        // token A → token B is the zero_for_one direction; limit 0 lets the pool
        // walk down to its own bound.
        client.submit_order_cl(
            &trader,
            &cl_pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &true,
            &0_u128,
            &None,
            &far_future_deadline(&env),
        );

        env.ledger().set_timestamp(1031);

        let results = client.settle_batch();
        assert_eq!(results.len(), 1);
        assert!(results.get(0).unwrap() > 0);

        // Trader received token_b from the CL pool.
        assert!(StellarTokenClient::new(&env, &tb).balance(&trader) > 0);
        // Escrowed token_a was fully consumed by the swap.
        assert_eq!(
            StellarTokenClient::new(&env, &ta).balance(&trader),
            90_000_i128
        );
    }

    #[test]
    fn test_quote_and_settle_route_to_best_venue() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let admin = Address::generate(&env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        // Two venues over the same pair: a constant-product AMM and a CL pool.
        let amm_pool = deploy_pool(&env, &ta, &tb);
        let lp = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&lp, &2_000_000_i128);
        StellarAssetClient::new(&env, &tb).mint(&lp, &2_000_000_i128);
        AmmPoolClient::new(&env, &amm_pool).add_liquidity(
            &lp,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );
        let cl_pool = deploy_cl_pool(&env, &admin, &ta, &tb);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.add_venue(&admin, &cl_pool, &PoolType::Cl);
        client.add_venue(&admin, &amm_pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        // CL order with the AMM pool as alternate venue.
        let order_id = client.submit_order_cl(
            &trader,
            &cl_pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &true,
            &0_u128,
            &Some(amm_pool.clone()),
            &far_future_deadline(&env),
        );

        // Independently quote both venues; the best of the two must match the
        // contract's chosen quote.
        let amm_q = AmmPoolClient::new(&env, &amm_pool).get_amount_out(&ta, &10_000_i128);
        let cl_q = ConcentratedLiquidityClient::new(&env, &cl_pool)
            .estimate_price_impact(&true, &10_000_i128, &0_u128)
            .amount_out;
        let expected_best = amm_q.max(cl_q);
        let expected_pool = if amm_q > cl_q {
            amm_pool.clone()
        } else {
            cl_pool.clone()
        };

        let (best_out, best_pool, _ptype) = client.quote_order(&order_id);
        assert_eq!(best_out, expected_best);
        assert_eq!(best_pool, expected_pool);

        env.ledger().set_timestamp(1031);
        let results = client.settle_batch();
        assert_eq!(results.len(), 1);
        // Realized output is at least the best quote's min_out and positive.
        assert!(results.get(0).unwrap() > 0);
        assert!(StellarTokenClient::new(&env, &tb).balance(&trader) > 0);
    }

    #[test]
    fn test_submit_order_cl_rejects_alt_pool_with_wrong_pair() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let admin = Address::generate(&env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tc = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_pool = deploy_cl_pool(&env, &admin, &ta, &tb);
        let wrong_amm_pool = deploy_pool(&env, &ta, &tc);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.add_venue(&admin, &cl_pool, &PoolType::Cl);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        let result = client.try_submit_order_cl(
            &trader,
            &cl_pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &true,
            &0_u128,
            &Some(wrong_amm_pool),
            &far_future_deadline(&env),
        );
        assert_eq!(result, Err(Ok(AuctionError::InvalidPoolTokenPair)));
        assert_eq!(
            StellarTokenClient::new(&env, &ta).balance(&trader),
            100_000_i128
        );
        assert_eq!(client.get_pending_orders().len(), 0);
    }

    #[test]
    fn test_quote_order_ignores_stored_alt_pool_with_wrong_pair() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let admin = Address::generate(&env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tc = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let primary_pool = deploy_cl_pool(&env, &admin, &ta, &tb);
        let wrong_alt_pool = deploy_pool(&env, &ta, &tc);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);

        let trader = Address::generate(&env);
        let order = Order {
            id: 7,
            trader,
            pool: primary_pool.clone(),
            token_in: ta.clone(),
            token_out: tb.clone(),
            amount_in: 10_000_i128,
            min_out: 0_i128,
            submitted_at: 1000,
            deadline: u64::MAX,
            pool_type: PoolType::Cl,
            zero_for_one: true,
            sqrt_price_limit: 0_u128,
            alt_pool: Some(wrong_alt_pool),
        };
        env.as_contract(&auction_addr, || {
            env.storage()
                .persistent()
                .set(&DataKey::Order(order.id), &order);
        });

        let (best_out, best_pool, best_type) = client.quote_order(&order.id);
        let primary_q = ConcentratedLiquidityClient::new(&env, &primary_pool)
            .estimate_price_impact(&true, &10_000_i128, &0_u128)
            .amount_out;
        assert_eq!(best_out, primary_q);
        assert_eq!(best_pool, primary_pool);
        assert_eq!(best_type, PoolType::Cl);
    }

    #[test]
    fn test_amm_order_still_defaults_to_amm_pool_type() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);
        let id = client.submit_order(
            &trader,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        let order = client.get_pending_orders().get(0).unwrap();
        assert_eq!(order.pool_type, PoolType::Amm);
        assert!(order.alt_pool.is_none());

        // The quote resolves through the AMM venue.
        let (best_out, best_pool, ptype) = client.quote_order(&id);
        assert_eq!(best_pool, pool);
        assert_eq!(ptype, PoolType::Amm);
        assert!(best_out > 0);
    }

    #[test]
    fn test_partial_settlement_refreshes_batch_opened_at() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.set_max_orders(&admin, &2_u32);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let trader1 = Address::generate(&env);
        let trader2 = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader1, &100_000_i128);
        StellarAssetClient::new(&env, &ta).mint(&trader2, &100_000_i128);

        client.submit_order(
            &trader1,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );
        client.submit_order(
            &trader2,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        // Increase max_orders so we can submit a 3rd order, but set max_orders to 1 before settlement
        client.set_max_orders(&admin, &1_u32);

        // Advance timestamp past window (1000 + 30 = 1030)
        env.ledger().set_timestamp(1030);

        let results = client.settle_batch();
        assert_eq!(results.len(), 1);
        assert_eq!(client.get_pending_orders().len(), 1);

        let (_pending_count, _max_orders, opened_at, _window_secs) = client.get_batch_info();
        assert_eq!(
            opened_at, 1030,
            "BatchOpenedAt must be refreshed to timestamp of settlement"
        );

        // Submit another order at timestamp 1035
        env.ledger().set_timestamp(1035);
        client.set_max_orders(&admin, &5_u32);
        let trader3 = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader3, &100_000_i128);
        client.submit_order(
            &trader3,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        // Attempting settle_batch at t=1035 must fail because opened_at is 1030 and window is 30s
        let err = client.try_settle_batch().err().unwrap().unwrap();
        assert_eq!(err, AuctionError::BatchWindowOpen);
    }

    // ── Issue #546: transfer failure isolation ──────────────────────────────────

    /// Verify that a swap failure followed by a *successful* refund still
    /// isolates the order (issue #473) and that `try_transfer` wrapping on
    /// the refund leg (issue #546) is exercised. The order is dropped and
    /// the batch continues.
    #[test]
    fn test_swap_failure_refund_via_try_transfer() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let bad_trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&bad_trader, &10_000_i128);

        client.submit_order(
            &bad_trader,
            &pool,
            &ta,
            &tb,
            &5_000_i128,
            &1_000_000_000_i128, // impossible min_out
            &far_future_deadline(&env),
        );

        env.ledger().set_timestamp(1031);

        let results = client.settle_batch();
        assert_eq!(results.len(), 0);

        // Refund went through (try_transfer succeeded here).
        assert_eq!(
            StellarTokenClient::new(&env, &ta).balance(&bad_trader),
            10_000_i128
        );
        assert_eq!(client.get_pending_orders().len(), 0);
    }

    /// Issue #546: payout transfer failure isolation.
    ///
    /// Submit two orders. Before settlement, modify the first order's
    /// `token_out` in storage to point at a non-existent contract. The swap
    /// succeeds (AMM swap doesn't reference `token_out` directly) but the
    /// payout `try_transfer` traps on the fake address. The failing order is
    /// dropped and the second order still settles — proving a single bad
    /// payout cannot revert the entire batch.
    #[test]
    fn test_failed_payout_does_not_revert_batch() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.set_max_orders(&admin, &2_u32);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let bad_trader = Address::generate(&env);
        let good_trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&bad_trader, &100_000_i128);
        StellarAssetClient::new(&env, &ta).mint(&good_trader, &100_000_i128);

        // Both orders submit with valid tokens — escrow succeeds.
        client.submit_order(
            &bad_trader,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );
        client.submit_order(
            &good_trader,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        // Mutate the bad order's token_out to a non-existent contract
        // address so that the payout try_transfer will trap.
        let fake_token_out = Address::generate(&env);
        env.as_contract(&auction_addr, || {
            let mut bad_order: Order = env.storage().persistent().get(&DataKey::Order(0)).unwrap();
            bad_order.token_out = fake_token_out.clone();
            env.storage()
                .persistent()
                .set(&DataKey::Order(0), &bad_order);
        });

        env.ledger().set_timestamp(1031);

        // Batch must NOT revert — the good order settles despite the bad
        // order's payout transfer failure.
        let results = client.settle_batch();
        assert_eq!(results.len(), 1);
        assert!(results.get(0).unwrap() > 0);
        assert!(StellarTokenClient::new(&env, &tb).balance(&good_trader) > 0);

        // Both orders are cleared from the batch.
        assert_eq!(client.get_pending_orders().len(), 0);

        // The stranded payout is recorded as claimable (issue #700) rather
        // than lost outright.
        let claimable: (Address, Address, i128) = env.as_contract(&auction_addr, || {
            env.storage()
                .persistent()
                .get(&DataKey::Claimable(0))
                .unwrap()
        });
        assert_eq!(claimable.0, bad_trader);
        assert_eq!(claimable.1, fake_token_out);
        assert!(claimable.2 > 0);
    }

    /// Issue #546: deadline-expiry refund transfer failure isolation.
    ///
    /// Submit an order with a short deadline that has already passed and
    /// mutate its `token_in` to a non-existent contract address before
    /// settlement. The order hits the deadline-expired path and the refund
    /// `try_transfer` traps on the fake address. The batch must complete
    /// without reverting.
    #[test]
    fn test_failed_deadline_refund_does_not_revert_batch() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        // Submit an order with a short deadline (1010) that will expire.
        client.submit_order(&trader, &pool, &ta, &tb, &10_000_i128, &0_i128, &1010_u64);

        // Mutate token_in to a non-existent contract so the deadline-expiry
        // refund try_transfer traps.
        let fake_token_in = Address::generate(&env);
        env.as_contract(&auction_addr, || {
            let mut order: Order = env.storage().persistent().get(&DataKey::Order(0)).unwrap();
            order.token_in = fake_token_in.clone();
            env.storage().persistent().set(&DataKey::Order(0), &order);
        });

        // Advance past the batch window AND the deadline.
        env.ledger().set_timestamp(1040);

        // Batch must NOT revert — the expired order is dropped despite the
        // refund transfer failure.
        let results = client.settle_batch();
        assert_eq!(results.len(), 0);
        assert_eq!(client.get_pending_orders().len(), 0);

        // The stranded refund is recorded as claimable (issue #700) rather
        // than lost outright.
        let claimable: (Address, Address, i128) = env.as_contract(&auction_addr, || {
            env.storage()
                .persistent()
                .get(&DataKey::Claimable(0))
                .unwrap()
        });
        assert_eq!(claimable, (trader, fake_token_in, 10_000_i128));
    }

    // ── Issue #553: two-step admin rotation ─────────────────────────────────────

    #[test]
    fn test_propose_and_accept_admin() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let nominee = Address::generate(&env);
        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);

        assert_eq!(client.get_admin(), admin);
        assert_eq!(client.get_pending_admin(), None);

        client.propose_admin(&admin, &nominee);
        assert_eq!(client.get_pending_admin(), Some(nominee.clone()));
        // Admin is unchanged until accept.
        assert_eq!(client.get_admin(), admin);

        client.accept_admin(&nominee);
        assert_eq!(client.get_admin(), nominee);
        assert_eq!(client.get_pending_admin(), None);

        // New admin can call admin-only setters; old admin cannot.
        client.set_max_orders(&nominee, &10_u32);
        let err = client
            .try_set_max_orders(&admin, &5_u32)
            .err()
            .unwrap()
            .unwrap();
        assert_eq!(err, AuctionError::Unauthorized);
    }

    #[test]
    fn test_propose_admin_requires_current_admin() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let rando = Address::generate(&env);
        let nominee = Address::generate(&env);
        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);

        let err = client
            .try_propose_admin(&rando, &nominee)
            .err()
            .unwrap()
            .unwrap();
        assert_eq!(err, AuctionError::Unauthorized);
    }

    #[test]
    fn test_accept_admin_requires_pending_nominee() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let nominee = Address::generate(&env);
        let other = Address::generate(&env);
        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);

        let err = client.try_accept_admin(&nominee).err().unwrap().unwrap();
        assert_eq!(err, AuctionError::NoPendingAdmin);

        client.propose_admin(&admin, &nominee);
        let err = client.try_accept_admin(&other).err().unwrap().unwrap();
        assert_eq!(err, AuctionError::WrongAdmin);
    }

    #[test]
    fn test_new_admin_controls_batch_window_after_acceptance() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let nominee = Address::generate(&env);
        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);

        client.propose_admin(&admin, &nominee);
        client.accept_admin(&nominee);

        // Nominee (new admin) can update the window; auth is checked against
        // the stored admin inside set_batch_window.
        client.set_batch_window(&60_u64);
        let (_, _, _, window) = client.get_batch_info();
        assert_eq!(window, 60);
    }

    // ── Issue #700: venue registry ──────────────────────────────────────────────

    #[test]
    fn test_hostile_venue_rejected_before_escrow() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let admin = Address::generate(&env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let hostile_addr = env.register_contract(None, HostileVenue);
        HostileVenueClient::new(&env, &hostile_addr).initialize(&ta, &tb);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        // Deliberately no add_venue / set_factory — hostile_addr is unregistered.

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &10_000_i128);

        let result = client.try_submit_order(
            &trader,
            &hostile_addr,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );
        assert_eq!(result, Err(Ok(AuctionError::UnknownVenue)));

        // No escrow happened — the trader keeps every token, and the
        // hostile contract's own fabricated swap was never reachable.
        assert_eq!(
            StellarTokenClient::new(&env, &ta).balance(&trader),
            10_000_i128
        );
        assert_eq!(client.get_pending_orders().len(), 0);
    }

    #[test]
    fn test_hostile_alt_pool_rejected_as_strictly_as_primary() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let admin = Address::generate(&env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_pool = deploy_cl_pool(&env, &admin, &ta, &tb);

        let hostile_addr = env.register_contract(None, HostileVenue);
        HostileVenueClient::new(&env, &hostile_addr).initialize(&ta, &tb);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        // Only the primary CL venue is legitimate; the alt (Amm-typed)
        // venue is the unregistered hostile contract.
        client.add_venue(&admin, &cl_pool, &PoolType::Cl);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &10_000_i128);

        let result = client.try_submit_order_cl(
            &trader,
            &cl_pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &true,
            &0_u128,
            &Some(hostile_addr),
            &far_future_deadline(&env),
        );
        assert_eq!(result, Err(Ok(AuctionError::UnknownVenue)));
        assert_eq!(
            StellarTokenClient::new(&env, &ta).balance(&trader),
            10_000_i128
        );
        assert_eq!(client.get_pending_orders().len(), 0);
    }

    #[test]
    fn test_removed_venue_refunds_order_others_still_settle() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        // A second, independent AMM venue for the same pair.
        let pool2 = deploy_pool(&env, &ta, &tb);
        let lp2 = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&lp2, &2_000_000_i128);
        StellarAssetClient::new(&env, &tb).mint(&lp2, &2_000_000_i128);
        AmmPoolClient::new(&env, &pool2).add_liquidity(
            &lp2,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.add_venue(&admin, &pool, &PoolType::Amm);
        client.add_venue(&admin, &pool2, &PoolType::Amm);

        let trader1 = Address::generate(&env);
        let trader2 = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader1, &10_000_i128);
        StellarAssetClient::new(&env, &ta).mint(&trader2, &10_000_i128);

        client.submit_order(
            &trader1,
            &pool,
            &ta,
            &tb,
            &5_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );
        client.submit_order(
            &trader2,
            &pool2,
            &ta,
            &tb,
            &5_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        // Admin removes trader1's venue before settlement.
        client.remove_venue(&admin, &pool);

        env.ledger().set_timestamp(1031);
        let results = client.settle_batch();

        // Only trader2's order (pool2, still valid) settled.
        assert_eq!(results.len(), 1);
        assert!(results.get(0).unwrap() > 0);

        // trader1's escrow was refunded, not swapped against the removed venue.
        assert_eq!(
            StellarTokenClient::new(&env, &ta).balance(&trader1),
            10_000_i128
        );
        assert_eq!(StellarTokenClient::new(&env, &tb).balance(&trader1), 0);
        assert!(StellarTokenClient::new(&env, &tb).balance(&trader2) > 0);
        assert_eq!(client.get_pending_orders().len(), 0);
    }

    #[test]
    fn test_venue_allowlist_management_and_pagination() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let admin = Address::generate(&env);
        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);

        let p1 = Address::generate(&env);
        let p2 = Address::generate(&env);
        let p3 = Address::generate(&env);

        assert!(!client.is_venue_allowed(&p1));
        client.add_venue(&admin, &p1, &PoolType::Amm);
        client.add_venue(&admin, &p2, &PoolType::Cl);
        client.add_venue(&admin, &p3, &PoolType::Amm);
        assert!(client.is_venue_allowed(&p1));

        assert_eq!(client.list_venues(&0_u32, &10_u32).len(), 3);
        assert_eq!(client.list_venues(&0_u32, &2_u32).len(), 2);
        assert_eq!(client.list_venues(&2_u32, &2_u32).len(), 1);

        client.remove_venue(&admin, &p2);
        assert!(!client.is_venue_allowed(&p2));
        assert_eq!(client.list_venues(&0_u32, &10_u32).len(), 2);

        // Non-admin cannot add or remove venues.
        let rando = Address::generate(&env);
        let err = client
            .try_add_venue(&rando, &p1, &PoolType::Amm)
            .err()
            .unwrap()
            .unwrap();
        assert_eq!(err, AuctionError::Unauthorized);
        let err = client.try_remove_venue(&rando, &p1).err().unwrap().unwrap();
        assert_eq!(err, AuctionError::Unauthorized);
    }

    #[test]
    fn test_add_venue_rejects_past_max_venues_and_recovers_after_remove() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);
        // Filling the allowlist to MAX_VENUES makes 200 contract calls in one
        // test, which exceeds the default test-env CPU budget well before
        // hitting any real resource limit.
        env.budget().reset_unlimited();

        let admin = Address::generate(&env);
        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);

        let mut pools: std::vec::Vec<Address> = std::vec::Vec::new();
        for _ in 0..MAX_VENUES {
            let pool = Address::generate(&env);
            client.add_venue(&admin, &pool, &PoolType::Amm);
            pools.push(pool);
        }
        assert_eq!(client.list_venues(&0_u32, &MAX_VENUES).len(), MAX_VENUES);

        let one_too_many = Address::generate(&env);
        let err = client
            .try_add_venue(&admin, &one_too_many, &PoolType::Amm)
            .err()
            .unwrap()
            .unwrap();
        assert_eq!(err, AuctionError::TooManyVenues);

        // Freeing a slot lets a new venue in again.
        client.remove_venue(&admin, pools.first().unwrap());
        client.add_venue(&admin, &one_too_many, &PoolType::Amm);
        assert!(client.is_venue_allowed(&one_too_many));
    }

    #[test]
    fn test_set_factory_is_admin_gated_and_attests_cl_venue_without_allowlist() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

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

        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let tb = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_pool = factory.create_cl_pool(&admin, &ta, &tb, &30_i128, &0_i32);

        // Seed liquidity directly on the deployed pool.
        let lp = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&lp, &100_000_000_i128);
        StellarAssetClient::new(&env, &tb).mint(&lp, &100_000_000_i128);
        ConcentratedLiquidityClient::new(&env, &cl_pool).mint_position(
            &lp,
            &-1_000_i32,
            &1_000_i32,
            &50_000_000_i128,
            &50_000_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);

        // set_factory is admin-gated.
        let rando = Address::generate(&env);
        let err = client
            .try_set_factory(&rando, &factory_addr)
            .err()
            .unwrap()
            .unwrap();
        assert_eq!(err, AuctionError::Unauthorized);

        client.set_factory(&admin, &factory_addr);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);

        // Succeeds purely via factory attestation — no admin allowlist entry.
        assert!(!client.is_venue_allowed(&cl_pool));
        client.submit_order_cl(
            &trader,
            &cl_pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &true,
            &0_u128,
            &None,
            &far_future_deadline(&env),
        );
        assert_eq!(client.get_pending_orders().len(), 1);
    }

    #[test]
    fn test_submit_order_rejects_deadline_too_far() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);
        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &20_000_i128);

        let too_far = 1_000_u64 + 7 * 24 * 60 * 60 + 1;
        let result =
            client.try_submit_order(&trader, &pool, &ta, &tb, &5_000_i128, &0_i128, &too_far);
        assert_eq!(result, Err(Ok(AuctionError::DeadlineTooFar)));
        assert_eq!(
            StellarTokenClient::new(&env, &ta).balance(&trader),
            20_000_i128
        );

        // Right at the boundary must still succeed.
        let ok_deadline = 1_000_u64 + 7 * 24 * 60 * 60;
        client.submit_order(&trader, &pool, &ta, &tb, &5_000_i128, &0_i128, &ok_deadline);
        assert_eq!(client.get_pending_orders().len(), 1);
    }

    #[test]
    fn test_expire_order_lifecycle() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);
        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        // Long batch window so settle_batch itself would never fire in this test.
        client.initialize(&admin, &1_000_u64);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &10_000_i128);
        let order_id =
            client.submit_order(&trader, &pool, &ta, &tb, &5_000_i128, &0_i128, &1_010_u64);

        // Not yet expired.
        let err = client.try_expire_order(&order_id).err().unwrap().unwrap();
        assert_eq!(err, AuctionError::OrderNotExpired);

        env.ledger().set_timestamp(1_020);
        client.expire_order(&order_id);
        assert_eq!(
            StellarTokenClient::new(&env, &ta).balance(&trader),
            10_000_i128
        );
        assert_eq!(client.get_pending_orders().len(), 0);

        // Idempotent: a second call errors rather than double-refunding.
        let err = client.try_expire_order(&order_id).err().unwrap().unwrap();
        assert_eq!(err, AuctionError::OrderNotFound);
        assert_eq!(
            StellarTokenClient::new(&env, &ta).balance(&trader),
            10_000_i128
        );
    }

    #[test]
    fn test_get_expired_orders_returns_only_expired() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);
        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &1_000_u64);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let t1 = Address::generate(&env);
        let t2 = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&t1, &10_000_i128);
        StellarAssetClient::new(&env, &ta).mint(&t2, &10_000_i128);

        let id1 = client.submit_order(&t1, &pool, &ta, &tb, &1_000_i128, &0_i128, &1_010_u64);
        let id2 = client.submit_order(
            &t2,
            &pool,
            &ta,
            &tb,
            &1_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        env.ledger().set_timestamp(1_020);

        let expired = client.get_expired_orders();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired.get(0).unwrap(), id1);
        let _ = id2;
    }

    #[test]
    fn test_claim_refund_pays_out_and_clears_claimable() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let admin = Address::generate(&env);
        let ta = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);

        let trader = Address::generate(&env);
        // Seed the contract with real balance and a claimable record
        // directly, simulating what settle_batch/expire_order leave behind
        // after a failed transfer (that failure path itself is covered by
        // test_failed_payout_does_not_revert_batch /
        // test_failed_deadline_refund_does_not_revert_batch below); this
        // isolates claim_refund's own payout/authorization/clearing logic.
        StellarAssetClient::new(&env, &ta).mint(&auction_addr, &5_000_i128);
        env.as_contract(&auction_addr, || {
            env.storage().persistent().set(
                &DataKey::Claimable(42_u64),
                &(trader.clone(), ta.clone(), 5_000_i128),
            );
        });

        // Wrong caller cannot claim.
        let rando = Address::generate(&env);
        let err = client
            .try_claim_refund(&rando, &42_u64)
            .err()
            .unwrap()
            .unwrap();
        assert_eq!(err, AuctionError::Unauthorized);

        let claimed = client.claim_refund(&trader, &42_u64);
        assert_eq!(claimed, 5_000_i128);
        assert_eq!(
            StellarTokenClient::new(&env, &ta).balance(&trader),
            5_000_i128
        );

        // Already claimed — nothing left.
        let err = client
            .try_claim_refund(&trader, &42_u64)
            .err()
            .unwrap()
            .unwrap();
        assert_eq!(err, AuctionError::NothingToClaim);
    }

    #[test]
    fn test_escrow_conservation_across_mixed_batch_outcomes() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let pool2 = deploy_pool(&env, &ta, &tb);
        let lp2 = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&lp2, &2_000_000_i128);
        StellarAssetClient::new(&env, &tb).mint(&lp2, &2_000_000_i128);
        AmmPoolClient::new(&env, &pool2).add_liquidity(
            &lp2,
            &1_000_000_i128,
            &1_000_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.add_venue(&admin, &pool, &PoolType::Amm);
        client.add_venue(&admin, &pool2, &PoolType::Amm);

        // Order A: settles normally against `pool`.
        let trader_a = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader_a, &10_000_i128);
        client.submit_order(
            &trader_a,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        // Order B: expires before settlement.
        let trader_b = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader_b, &10_000_i128);
        client.submit_order(
            &trader_b,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &1_010_u64,
        );

        // Order C: its venue (`pool2`) is removed before settlement.
        let trader_c = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader_c, &10_000_i128);
        client.submit_order(
            &trader_c,
            &pool2,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );
        client.remove_venue(&admin, &pool2);

        // Order D: left pending — the batch cap is lowered below the pending
        // count right before settlement so it is not processed this round.
        let trader_d = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader_d, &10_000_i128);
        client.submit_order(
            &trader_d,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        env.ledger().set_timestamp(1031);
        client.set_max_orders(&admin, &3_u32);
        let results = client.settle_batch();
        // A actually settles (real output); B's expiry also successfully
        // refunds and is recorded as a `0` output (pre-existing behavior);
        // C (venue removed) and D (left pending) contribute nothing here.
        assert_eq!(results.len(), 2);
        assert!(results.get(0).unwrap() > 0);
        assert_eq!(results.get(1).unwrap(), 0);
        assert_eq!(client.get_pending_orders().len(), 1); // D remains

        // The contract's real balances equal exactly D's still-escrowed
        // amount for token_a, and zero for token_b — nothing over-paid or
        // retained as dust.
        assert_eq!(
            StellarTokenClient::new(&env, &ta).balance(&auction_addr),
            10_000_i128
        );
        assert_eq!(
            StellarTokenClient::new(&env, &tb).balance(&auction_addr),
            0_i128
        );
    }

    #[test]
    fn test_order_submitted_emits_versioned_event() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);
        let _order_id = client.submit_order(
            &trader,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        // Find order_submitted event in all events
        let events = env.events().all();
        let order_submitted_events: std::vec::Vec<_> = events
            .iter()
            .filter(|event| {
                if let Some(topic_val) = event.1.get(0) {
                    Symbol::try_from_val(&env, &topic_val)
                        == Ok(Symbol::new(&env, "order_submitted"))
                } else {
                    false
                }
            })
            .collect();

        assert!(
            !order_submitted_events.is_empty(),
            "order_submitted event must be emitted"
        );
        let event = order_submitted_events.last().unwrap();
        let (version, (_order_id, _amount_in)): (u32, (u64, i128)) =
            <(u32, (u64, i128))>::try_from_val(&env, &event.2)
                .expect("must decode as (u32, (u64, i128))");
        assert_eq!(
            version,
            soroban_amm_sdk::EVENT_SCHEMA_VERSION,
            "event must have correct schema version"
        );
    }

    #[test]
    fn test_order_settled_emits_versioned_event() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);
        let _order_id = client.submit_order(
            &trader,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        // Advance time past the batch window
        env.ledger().set_timestamp(1031);

        let _results = client.settle_batch();

        // Find order_settled event
        let events = env.events().all();
        let order_settled_events: std::vec::Vec<_> = events
            .iter()
            .filter(|event| {
                if let Some(topic_val) = event.1.get(0) {
                    Symbol::try_from_val(&env, &topic_val) == Ok(Symbol::new(&env, "order_settled"))
                } else {
                    false
                }
            })
            .collect();

        assert!(
            !order_settled_events.is_empty(),
            "order_settled event must be emitted"
        );
        let event = order_settled_events.last().unwrap();
        let (version, (_order_id, _amount_out)): (u32, (u64, i128)) =
            <(u32, (u64, i128))>::try_from_val(&env, &event.2)
                .expect("must decode as (u32, (u64, i128))");
        assert_eq!(
            version,
            soroban_amm_sdk::EVENT_SCHEMA_VERSION,
            "event must have correct schema version"
        );
    }

    #[test]
    fn test_order_cancelled_emits_versioned_event() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);
        let order_id = client.submit_order(
            &trader,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        client.cancel_order(&trader, &order_id);

        // Find order_cancelled event
        let events = env.events().all();
        let order_cancelled_events: std::vec::Vec<_> = events
            .iter()
            .filter(|event| {
                if let Some(topic_val) = event.1.get(0) {
                    Symbol::try_from_val(&env, &topic_val)
                        == Ok(Symbol::new(&env, "order_cancelled"))
                } else {
                    false
                }
            })
            .collect();

        assert!(
            !order_cancelled_events.is_empty(),
            "order_cancelled event must be emitted"
        );
        let event = order_cancelled_events.last().unwrap();
        let (version, (_order_id,)): (u32, (u64,)) =
            <(u32, (u64,))>::try_from_val(&env, &event.2).expect("must decode as (u32, (u64,))");
        assert_eq!(
            version,
            soroban_amm_sdk::EVENT_SCHEMA_VERSION,
            "event must have correct schema version"
        );
    }

    #[test]
    fn test_admin_changed_emits_versioned_event() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (_ta, _tb, _pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);

        let new_admin = Address::generate(&env);
        client.propose_admin(&admin, &new_admin);
        client.accept_admin(&new_admin);

        // Find admin_changed event
        let events = env.events().all();
        let admin_changed_events: std::vec::Vec<_> = events
            .iter()
            .filter(|event| {
                if let Some(topic_val) = event.1.get(0) {
                    Symbol::try_from_val(&env, &topic_val) == Ok(Symbol::new(&env, "admin_changed"))
                } else {
                    false
                }
            })
            .collect();

        assert!(
            !admin_changed_events.is_empty(),
            "admin_changed event must be emitted"
        );
        let event = admin_changed_events.last().unwrap();
        let (version, (_new_admin,)): (u32, (Address,)) =
            <(u32, (Address,))>::try_from_val(&env, &event.2)
                .expect("must decode as (u32, (Address,))");
        assert_eq!(
            version,
            soroban_amm_sdk::EVENT_SCHEMA_VERSION,
            "event must have correct schema version"
        );
    }

    #[test]
    fn test_settled_event_emits_versioned_event() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);
        let _order_id = client.submit_order(
            &trader,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );

        // Advance time past the batch window
        env.ledger().set_timestamp(1031);

        let _results = client.settle_batch();

        // Find settled event
        let events = env.events().all();
        let settled_events: std::vec::Vec<_> = events
            .iter()
            .filter(|event| {
                if let Some(topic_val) = event.1.get(0) {
                    Symbol::try_from_val(&env, &topic_val) == Ok(Symbol::new(&env, "settled"))
                } else {
                    false
                }
            })
            .collect();

        assert!(!settled_events.is_empty(), "settled event must be emitted");
        let event = settled_events.last().unwrap();
        let (version, (_process_count,)): (u32, (u32,)) =
            <(u32, (u32,))>::try_from_val(&env, &event.2).expect("must decode as (u32, (u32,))");
        assert_eq!(
            version,
            soroban_amm_sdk::EVENT_SCHEMA_VERSION,
            "event must have correct schema version"
        );
    }

    // ── Issue #922: no event escapes the version stamp ────────────────────────
    //
    // Rather than one assertion per topic, this walks the whole event log for a
    // representative call sequence and asserts every payload this contract
    // emitted begins with `EVENT_SCHEMA_VERSION`. A newly added raw
    // `env.events().publish(...)` reachable from the sequence regresses this
    // test, which is the guard the half-migrated state lacked.
    #[test]
    fn test_every_emitted_event_carries_version_stamp() {
        use soroban_sdk::Val;

        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);

        let (ta, tb, pool, admin) = setup(&env);

        let auction_addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &auction_addr);
        client.initialize(&admin, &30_u64);

        // Admin-config surface: window_updated, max_orders_updated,
        // factory_updated, venue_added.
        client.set_batch_window(&45_u64);
        client.set_max_orders(&admin, &50_u32);
        let factory = Address::generate(&env);
        client.set_factory(&admin, &factory);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        // Order lifecycle: order_submitted, order_settled, settled.
        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &100_000_i128);
        client.submit_order(
            &trader,
            &pool,
            &ta,
            &tb,
            &10_000_i128,
            &0_i128,
            &far_future_deadline(&env),
        );
        env.ledger().set_timestamp(1046);
        let _ = client.settle_batch();

        // Teardown + admin rotation: venue_removed, admin_nominated,
        // admin_changed.
        client.remove_venue(&admin, &pool);
        let new_admin = Address::generate(&env);
        client.propose_admin(&admin, &new_admin);
        client.accept_admin(&new_admin);

        // Every event this contract published must decode as `(u32, _)` with the
        // leading `u32` equal to the current schema version. Events emitted by
        // the pool/token contracts are filtered out by address.
        let mut checked = 0u32;
        for event in env.events().all().iter() {
            if event.0 != auction_addr {
                continue;
            }
            let payload: soroban_sdk::Vec<Val> = soroban_sdk::Vec::try_from_val(&env, &event.2)
                .expect("every auction event payload is a versioned tuple");
            let head = payload.get(0).expect("versioned payload is never empty");
            let version = u32::try_from_val(&env, &head)
                .expect("leading element of every auction event is the u32 version");
            assert_eq!(
                version,
                soroban_amm_sdk::EVENT_SCHEMA_VERSION,
                "event with topics {:?} is missing the schema version stamp",
                event.1
            );
            checked += 1;
        }
        assert!(
            checked >= 7,
            "representative sequence should emit several auction events, saw {checked}"
        );
    }

    // ── #1119: window bounds, NotInitialized, TTL, list_venues ────────────────

    fn init_client(env: &Env, window: u64) -> (BatchAuctionClient<'_>, Address, Address) {
        let admin = Address::generate(env);
        let addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(env, &addr);
        client.initialize(&admin, &window);
        (client, addr, admin)
    }

    #[test]
    fn test_initialize_rejects_out_of_range_windows() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        for bad in [
            0_u64,
            MIN_BATCH_WINDOW_SECS - 1,
            MAX_BATCH_WINDOW_SECS + 1,
            u64::MAX,
        ] {
            let addr = env.register_contract(None, BatchAuction);
            let client = BatchAuctionClient::new(&env, &addr);
            assert_eq!(
                client.try_initialize(&admin, &bad),
                Err(Ok(AuctionError::InvalidBatchWindow)),
                "window {bad} must be rejected"
            );
            // A rejected initialize leaves the contract uninitialized.
            assert_eq!(
                client.try_get_admin(),
                Err(Ok(AuctionError::NotInitialized))
            );
        }
        for good in [MIN_BATCH_WINDOW_SECS, 30, MAX_BATCH_WINDOW_SECS] {
            let addr = env.register_contract(None, BatchAuction);
            let client = BatchAuctionClient::new(&env, &addr);
            client.initialize(&admin, &good);
            assert_eq!(client.get_batch_info().3, good);
        }
    }

    #[test]
    fn test_set_batch_window_rejects_zero_and_u64_max() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, _) = init_client(&env, 30);

        assert_eq!(
            client.try_set_batch_window(&0_u64),
            Err(Ok(AuctionError::InvalidBatchWindow))
        );
        assert_eq!(
            client.try_set_batch_window(&u64::MAX),
            Err(Ok(AuctionError::InvalidBatchWindow))
        );
        assert_eq!(
            client.try_set_batch_window(&(MAX_BATCH_WINDOW_SECS + 1)),
            Err(Ok(AuctionError::InvalidBatchWindow))
        );
        // The rejected calls changed nothing.
        assert_eq!(client.get_batch_info().3, 30);
    }

    #[test]
    fn test_set_batch_window_accepts_both_edges() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, _) = init_client(&env, 30);

        client.set_batch_window(&MIN_BATCH_WINDOW_SECS);
        assert_eq!(client.get_batch_info().3, MIN_BATCH_WINDOW_SECS);
        client.set_batch_window(&MAX_BATCH_WINDOW_SECS);
        assert_eq!(client.get_batch_info().3, MAX_BATCH_WINDOW_SECS);
    }

    #[test]
    fn test_settle_batch_cannot_overflow_with_the_largest_legal_window() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);
        let (client, addr, admin) = init_client(&env, 30);

        // The largest window the admin can configure at all.
        client.set_window_bounds(&admin, &MIN_BATCH_WINDOW_SECS, &WINDOW_BOUNDS_CEILING_SECS);
        client.set_batch_window(&WINDOW_BOUNDS_CEILING_SECS);
        assert_eq!(
            client.try_settle_batch(),
            Err(Ok(AuctionError::BatchWindowOpen))
        );

        // A window written by an older version can be anything; settlement
        // must return an error, not trap on the addition.
        env.as_contract(&addr, || {
            env.storage()
                .instance()
                .set(&DataKey::BatchWindowSecs, &u64::MAX);
            env.storage()
                .instance()
                .set(&DataKey::BatchOpenedAt, &(u64::MAX - 1));
        });
        assert_eq!(
            client.try_settle_batch(),
            Err(Ok(AuctionError::BatchWindowOpen))
        );
    }

    #[test]
    fn test_admin_entrypoints_return_not_initialized_before_initialize() {
        let env = Env::default();
        env.mock_all_auths();
        let addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &addr);
        let someone = Address::generate(&env);
        let pool = Address::generate(&env);
        assert_eq!(
            client.try_get_admin().err(),
            Some(Ok(AuctionError::NotInitialized))
        );
        let not_init = AuctionError::NotInitialized;
        assert_eq!(
            client.try_set_batch_window(&30_u64).err(),
            Some(Ok(not_init))
        );
        assert_eq!(
            client.try_set_max_orders(&someone, &10_u32).err(),
            Some(Ok(not_init))
        );
        assert_eq!(
            client.try_set_factory(&someone, &pool).err(),
            Some(Ok(not_init))
        );
        assert_eq!(
            client.try_add_venue(&someone, &pool, &PoolType::Amm).err(),
            Some(Ok(not_init))
        );
        assert_eq!(
            client.try_remove_venue(&someone, &pool).err(),
            Some(Ok(not_init))
        );
        assert_eq!(
            client.try_propose_admin(&someone, &pool).err(),
            Some(Ok(not_init))
        );
        assert_eq!(client.try_accept_admin(&someone).err(), Some(Ok(not_init)));
        assert_eq!(
            client
                .try_set_window_bounds(&someone, &5_u64, &60_u64)
                .err(),
            Some(Ok(not_init))
        );
        // Plain reads still work on an empty contract.
        assert_eq!(client.get_pending_admin(), None);
        assert_eq!(client.list_venues(&0, &10).len(), 0);
    }

    #[test]
    fn test_set_window_bounds_validation_and_effect() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, admin) = init_client(&env, 30);
        let stranger = Address::generate(&env);

        assert_eq!(
            client.get_window_bounds(),
            (MIN_BATCH_WINDOW_SECS, MAX_BATCH_WINDOW_SECS)
        );
        assert_eq!(
            client.try_set_window_bounds(&stranger, &5_u64, &60_u64),
            Err(Ok(AuctionError::Unauthorized))
        );
        for (min, max) in [
            (0_u64, 60_u64),
            (61, 60),
            (5, WINDOW_BOUNDS_CEILING_SECS + 1),
            (31, 60), // excludes the current window of 30
            (5, 29),  // excludes the current window of 30
        ] {
            assert_eq!(
                client.try_set_window_bounds(&admin, &min, &max),
                Err(Ok(AuctionError::InvalidWindowBounds)),
                "bounds ({min}, {max})"
            );
        }

        client.set_window_bounds(&admin, &10_u64, &120_u64);
        assert_eq!(client.get_window_bounds(), (10, 120));
        assert_eq!(
            client.try_set_batch_window(&9_u64),
            Err(Ok(AuctionError::InvalidBatchWindow))
        );
        assert_eq!(
            client.try_set_batch_window(&121_u64),
            Err(Ok(AuctionError::InvalidBatchWindow))
        );
        client.set_batch_window(&120_u64);
    }

    #[test]
    fn test_window_bounds_update_emits_versioned_event() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, admin) = init_client(&env, 30);

        client.set_window_bounds(&admin, &10_u64, &120_u64);

        let events = env.events().all();
        let event = events
            .iter()
            .rfind(|event| {
                event.1.get(0).map(|t| {
                    Symbol::try_from_val(&env, &t) == Ok(Symbol::new(&env, "window_bounds_updated"))
                }) == Some(true)
            })
            .expect("window_bounds_updated event must be emitted");
        let (version, (min, max)): (u32, (u64, u64)) =
            <(u32, (u64, u64))>::try_from_val(&env, &event.2)
                .expect("decodes as (u32, (u64, u64))");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!((min, max), (10, 120));
    }

    #[test]
    fn test_list_venues_with_out_of_range_offset_and_limit() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, admin) = init_client(&env, 30);
        let mut pools = std::vec::Vec::new();
        for _ in 0..3 {
            let pool = Address::generate(&env);
            client.add_venue(&admin, &pool, &PoolType::Amm);
            pools.push(pool);
        }

        assert_eq!(client.list_venues(&0, &u32::MAX).len(), 3);
        assert_eq!(client.list_venues(&1, &u32::MAX).len(), 2);
        assert_eq!(client.list_venues(&u32::MAX, &u32::MAX).len(), 0);
        assert_eq!(client.list_venues(&u32::MAX, &1).len(), 0);
        assert_eq!(client.list_venues(&3, &1).len(), 0);
        assert_eq!(client.list_venues(&1, &0).len(), 0);
        let page = client.list_venues(&1, &1);
        assert_eq!(page.len(), 1);
        assert_eq!(page.get(0).unwrap().0, pools[1]);
    }

    #[test]
    fn test_list_venues_skips_an_index_entry_without_a_record() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, addr, admin) = init_client(&env, 30);
        let kept = Address::generate(&env);
        let orphan = Address::generate(&env);
        client.add_venue(&admin, &orphan, &PoolType::Amm);
        client.add_venue(&admin, &kept, &PoolType::Cl);

        // Break the invariant directly: the index still lists `orphan`.
        env.as_contract(&addr, || {
            env.storage()
                .instance()
                .remove(&DataKey::Venue(orphan.clone()));
        });

        let listed = client.list_venues(&0, &10);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed.get(0).unwrap(), (kept, PoolType::Cl));
    }

    #[test]
    fn test_expire_order_with_failed_refund_is_claimable_by_the_trader_only_once() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1000);
        let (ta, tb, pool, admin) = setup(&env);
        let (client, addr, _) = {
            let addr = env.register_contract(None, BatchAuction);
            let client = BatchAuctionClient::new(&env, &addr);
            client.initialize(&admin, &1_000_u64);
            (client, addr, ())
        };
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &10_000_i128);
        let order_id =
            client.submit_order(&trader, &pool, &ta, &tb, &5_000_i128, &0_i128, &1_010_u64);

        // Make the refund transfer fail by pointing the order at a token that
        // is not a contract.
        let broken = Address::generate(&env);
        env.as_contract(&addr, || {
            let mut order: Order = env
                .storage()
                .persistent()
                .get(&DataKey::Order(order_id))
                .unwrap();
            order.token_in = broken.clone();
            env.storage()
                .persistent()
                .set(&DataKey::Order(order_id), &order);
        });

        env.ledger().set_timestamp(1_020);
        client.expire_order(&order_id);
        assert_eq!(
            client.get_claimable(&order_id),
            Some((trader.clone(), broken, 5_000_i128))
        );
        assert_eq!(client.get_pending_orders().len(), 0);

        // The obstacle is resolved (here: the record is pointed at the real
        // escrowed token) and the trader claims.
        env.as_contract(&addr, || {
            env.storage().persistent().set(
                &DataKey::Claimable(order_id),
                &(trader.clone(), ta.clone(), 5_000_i128),
            );
        });

        // Another address cannot claim it.
        let other = Address::generate(&env);
        assert_eq!(
            client.try_claim_refund(&other, &order_id),
            Err(Ok(AuctionError::Unauthorized))
        );
        // The trader can, once.
        let before = StellarTokenClient::new(&env, &ta).balance(&trader);
        assert_eq!(client.claim_refund(&trader, &order_id), 5_000_i128);
        assert_eq!(
            StellarTokenClient::new(&env, &ta).balance(&trader),
            before + 5_000
        );
        assert_eq!(
            client.try_claim_refund(&trader, &order_id),
            Err(Ok(AuctionError::NothingToClaim))
        );
    }

    /// Everything the TTL test needs to call any entrypoint successfully.
    struct TtlCtx {
        admin: Address,
        nominee: Address,
        pool: Address,
        trader: Address,
        order_id: u64,
        claim_id: u64,
    }

    fn instance_ttl(env: &Env, addr: &Address) -> u32 {
        use soroban_sdk::testutils::storage::Instance as _;
        env.as_contract(addr, || env.storage().instance().get_ttl())
    }

    /// Run `call` against a fresh contract whose instance TTL has lapsed below
    /// `MIN_TTL`, and assert the call restores it to `BUMP_TO`.
    fn assert_restores_ttl(name: &str, call: &dyn Fn(&Env, &BatchAuctionClient<'_>, &TtlCtx)) {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        // Entries start with a long TTL so the token, pool and order survive
        // the ledger jump below while the auction's instance falls under MIN_TTL.
        env.ledger().with_mut(|l| {
            l.timestamp = 1_000;
            l.min_persistent_entry_ttl = 700_000;
            l.max_entry_ttl = 6_312_000;
        });
        let (ta, tb, pool, admin) = setup(&env);
        let addr = env.register_contract(None, BatchAuction);
        let client = BatchAuctionClient::new(&env, &addr);
        client.initialize(&admin, &1_000_u64);
        client.add_venue(&admin, &pool, &PoolType::Amm);

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &ta).mint(&trader, &10_000_i128);
        let order_id =
            client.submit_order(&trader, &pool, &ta, &tb, &5_000_i128, &0_i128, &1_010_u64);
        let claim_id = 77_u64;
        StellarAssetClient::new(&env, &ta).mint(&addr, &1_000_i128);
        env.as_contract(&addr, || {
            env.storage().persistent().set(
                &DataKey::Claimable(claim_id),
                &(trader.clone(), ta.clone(), 1_000_i128),
            );
        });
        let nominee = Address::generate(&env);
        client.propose_admin(&admin, &nominee);
        let ctx = TtlCtx {
            admin,
            nominee,
            pool,
            trader,
            order_id,
            claim_id,
        };

        env.ledger().with_mut(|l| l.sequence_number += 600_000);
        assert!(
            instance_ttl(&env, &addr) < MIN_TTL,
            "{name}: the setup must lower the instance TTL below MIN_TTL"
        );

        call(&env, &client, &ctx);

        let ttl = instance_ttl(&env, &addr);
        assert!(
            ttl >= BUMP_TO - 1,
            "{name}: instance TTL {ttl} was not restored to {BUMP_TO}"
        );
    }

    #[test]
    fn test_every_public_entrypoint_restores_the_instance_ttl() {
        assert_restores_ttl("get_admin", &|_, c, _| {
            c.get_admin();
        });
        assert_restores_ttl("get_pending_admin", &|_, c, _| {
            c.get_pending_admin();
        });
        assert_restores_ttl("get_batch_info", &|_, c, _| {
            c.get_batch_info();
        });
        assert_restores_ttl("get_pending_orders", &|_, c, _| {
            c.get_pending_orders();
        });
        assert_restores_ttl("get_expired_orders", &|_, c, _| {
            c.get_expired_orders();
        });
        assert_restores_ttl("get_claimable", &|_, c, x| {
            c.get_claimable(&x.claim_id);
        });
        assert_restores_ttl("get_window_bounds", &|_, c, _| {
            c.get_window_bounds();
        });
        assert_restores_ttl("is_venue_allowed", &|_, c, x| {
            c.is_venue_allowed(&x.pool);
        });
        assert_restores_ttl("list_venues", &|_, c, _| {
            c.list_venues(&0, &10);
        });
        assert_restores_ttl("quote_order", &|_, c, x| {
            c.quote_order(&x.order_id);
        });
        assert_restores_ttl("set_batch_window", &|_, c, _| c.set_batch_window(&60_u64));
        assert_restores_ttl("set_window_bounds", &|_, c, x| {
            c.set_window_bounds(&x.admin, &5_u64, &3_600_u64)
        });
        assert_restores_ttl("set_max_orders", &|_, c, x| {
            c.set_max_orders(&x.admin, &10_u32)
        });
        assert_restores_ttl("set_factory", &|e, c, x| {
            c.set_factory(&x.admin, &Address::generate(e));
        });
        assert_restores_ttl("add_venue", &|e, c, x| {
            c.add_venue(&x.admin, &Address::generate(e), &PoolType::Amm)
        });
        assert_restores_ttl("remove_venue", &|_, c, x| c.remove_venue(&x.admin, &x.pool));
        assert_restores_ttl("propose_admin", &|e, c, x| {
            c.propose_admin(&x.admin, &Address::generate(e))
        });
        assert_restores_ttl("accept_admin", &|_, c, x| c.accept_admin(&x.nominee));
        assert_restores_ttl("expire_order", &|e, c, x| {
            e.ledger().set_timestamp(1_020);
            c.expire_order(&x.order_id)
        });
        assert_restores_ttl("claim_refund", &|_, c, x| {
            c.claim_refund(&x.trader, &x.claim_id);
        });
        assert_restores_ttl("cancel_order", &|_, c, x| {
            c.cancel_order(&x.trader, &x.order_id)
        });
    }
}
