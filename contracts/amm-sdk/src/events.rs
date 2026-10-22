//! Strongly-typed decoders for every event emitted by the AMM contracts.
//!
//! The AMM contract emits Soroban events with the following structure:
//!
//! | Event symbol    | Topics                        | Data                              |
//! |-----------------|-------------------------------|-----------------------------------|
//! | `swap`          | `("swap", trader)`            | `(1, (token_in, amt_in, token_out, amt_out))` (`swap`, `swap_exact_out`); `(1, (token_in, amt_in, token_out, amt_out, referrer))` (`swap_fot`) |
//! | `add_liquidity` | `("add_liquidity", provider)` | `(1, (amount_a, amount_b, shares))`    |
//! | `rm_liq`        | `("rm_liq",)`                 | `(1, (provider, shares, out_a, out_b))`|
//! | `rm_liq_1s`     | `("rm_liq_1s",)`              | `(1, (provider, shares, token_out, total_out))` |
//! | `flash_loan`    | `("flash_loan", receiver)`    | `(1, (amount_a, amount_b, fee_a, fee_b))` |
//! | `fee_upd`       | `("fee_upd", admin)`          | `(1, (new_fee_bps,))`                  |
//! | `flash_fee_upd` | `("flash_fee_upd", admin)`    | `(1, (new_fee_bps,))`                  |
//! | `admin_nominated`| `("admin_nominated",)`       | `(1, (current_admin, new_admin))`      |
//! | `admin_changed` | `("admin_changed",)`          | `(1, (new_admin,))`                    |
//! | `upgraded`      | `("upgraded",)`               | `(1, (new_wasm_hash,))`                |
//! | `protocol_fee_set` | `("protocol_fee_set",)`    | `(1, (protocol_fee_bps, recipient))`   |
//! | `circuit_break` | `("circuit_break",)`          | `(1, (price_before, price_after, deviation_bps, threshold_bps))` |
//! | `cb_recovered`  | `("cb_recovered",)`           | `(1, (timestamp,))`                    |
//!
//! The DEX aggregator (`contracts/dex_aggregator`) adds:
//!
//! | Event symbol | Topics          | Data                                    |
//! |--------------|-----------------|-----------------------------------------|
//! | `cl_reg`     | `("cl_reg",)`   | `(1, (token_a, token_b, fee_bps, pool))`|
//! | `route_sel`  | `("route_sel",)`| `(1, (venue, venue_kind, amount_in, amount_out))` |
//! | `route_alt`  | `("route_alt",)`| `(1, (venue, amount_out, alt_venue, alt_venue_kind, alt_amount_out))` |
//! | `route_exe`  | `("route_exe",)`| `(1, (trader, token_in, token_out, amount_in, amount_out, pool))` |
//! | `tol_fail`   | `("tol_fail",)` | `(1, (pool, observed_bps, tolerance_bps))` |

use soroban_sdk::{contracttype, Address, BytesN};

// ── Event data types ──────────────────────────────────────────────────────────

/// Emitted when a swap executes.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct SwapEvent {
    pub trader: Address,
    pub token_in: Address,
    pub amount_in: i128,
    pub token_out: Address,
    pub amount_out: i128,
    pub referrer: Option<Address>,
}

/// Emitted when liquidity is added.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct AddLiquidityEvent {
    pub provider: Address,
    pub amount_a: i128,
    pub amount_b: i128,
    pub shares_minted: i128,
}

/// Emitted when liquidity is removed (both tokens).
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct RemoveLiquidityEvent {
    pub provider: Address,
    pub shares_burned: i128,
    pub amount_a: i128,
    pub amount_b: i128,
}

/// Emitted when liquidity is removed as a single token.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct RemoveLiquidityOneSidedEvent {
    pub provider: Address,
    pub shares_burned: i128,
    pub token_out: Address,
    pub total_out: i128,
}

/// Emitted when a flash loan executes. The pool lends both tokens in one call,
/// so the event carries an amount and a fee for each side.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct FlashLoanEvent {
    pub receiver: Address,
    pub amount_a: i128,
    pub amount_b: i128,
    pub fee_a: i128,
    pub fee_b: i128,
}

/// Emitted when the swap fee is updated.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct FeeUpdatedEvent {
    pub admin: Address,
    pub new_fee_bps: i128,
}

/// Emitted when the flash loan fee is updated.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct FlashFeeUpdatedEvent {
    pub admin: Address,
    pub new_fee_bps: i128,
}

/// Emitted when a new admin is nominated.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct AdminNominatedEvent {
    pub current_admin: Address,
    pub new_admin: Address,
}

/// Emitted when admin transfer is accepted and completed.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct AdminChangedEvent {
    pub new_admin: Address,
}

/// Emitted when the contract WASM is upgraded.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct UpgradedEvent {
    pub new_wasm_hash: BytesN<32>,
}

/// Emitted when the protocol fee split is changed.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct ProtocolFeeSetEvent {
    pub protocol_fee_bps: i128,
    pub recipient: Address,
}

/// Emitted when the circuit breaker's cooldown elapses and the pool resumes
/// normal operation.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct CbRecoveredEvent {
    pub timestamp: u64,
}

/// Emitted when the circuit breaker auto-pauses the pool due to extreme price
/// movement.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct CircuitBreakerEvent {
    /// Spot price before the triggering trade (scaled × 1 000 000).
    pub price_before: i128,
    /// Spot price after the triggering trade (scaled × 1 000 000).
    pub price_after: i128,
    /// Measured deviation in basis points.
    pub deviation_bps: i128,
    /// Configured threshold that was exceeded.
    pub threshold_bps: i128,
}

/// Venue family behind a route hop. Mirrors `dex_aggregator::PoolKind`.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub enum RouteVenueKind {
    /// Constant-product (V2-style) AMM pool.
    Amm,
    /// Concentrated-liquidity pool.
    Cl,
}

/// Emitted when the aggregator registers a concentrated-liquidity pool.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct ClPoolRegisteredEvent {
    pub token_a: Address,
    pub token_b: Address,
    pub fee_bps: i128,
    pub pool: Address,
}

/// Emitted when the aggregator picks a route.
///
/// `venue` is the pool the route is entered through -- the venue that won the
/// first-hop decision.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct RouteSelectedEvent {
    pub venue: Address,
    pub venue_kind: RouteVenueKind,
    pub amount_in: i128,
    pub amount_out: i128,
}

/// Emitted alongside `RouteSelectedEvent` when a second venue also quoted the
/// trade, so the improvement the aggregator delivered is measurable.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct RouteAlternativeEvent {
    pub venue: Address,
    pub amount_out: i128,
    pub alt_venue: Address,
    pub alt_venue_kind: RouteVenueKind,
    pub alt_amount_out: i128,
}

/// Emitted after an aggregator route settles. `amount_out` is what the pools
/// actually returned, not the quote the route was planned against.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct RouteExecutedEvent {
    pub trader: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: i128,
    pub amount_out: i128,
    pub pool: Address,
}

/// Emitted when an aggregator price-tolerance check fails.
#[contracttype]
#[derive(Debug, Clone, PartialEq)]
pub struct ToleranceFailedEvent {
    pub pool: Address,
    pub observed_bps: i128,
    pub tolerance_bps: i128,
}

// ── Event symbol constants ────────────────────────────────────────────────────

/// Symbol strings matching the on-chain event topics.
pub mod symbols {
    pub const SWAP: &str = "swap";
    pub const ADD_LIQUIDITY: &str = "add_liquidity";
    pub const REMOVE_LIQUIDITY: &str = "rm_liq";
    pub const REMOVE_LIQUIDITY_ONE_SIDED: &str = "rm_liq_1s";
    pub const FLASH_LOAN: &str = "flash_loan";
    pub const FEE_UPDATED: &str = "fee_upd";
    pub const FLASH_FEE_UPDATED: &str = "flash_fee_upd";
    pub const ADMIN_NOMINATED: &str = "admin_nominated";
    pub const ADMIN_CHANGED: &str = "admin_changed";
    pub const UPGRADED: &str = "upgraded";
    pub const CIRCUIT_BREAKER: &str = "circuit_break";
    pub const PROTOCOL_FEE_SET: &str = "protocol_fee_set";
    pub const CB_RECOVERED: &str = "cb_recovered";

    // ── DEX aggregator (#685) ────────────────────────────────────────────────
    pub const CL_POOL_REGISTERED: &str = "cl_reg";
    pub const ROUTE_SELECTED: &str = "route_sel";
    pub const ROUTE_ALTERNATIVE: &str = "route_alt";
    pub const ROUTE_EXECUTED: &str = "route_exe";
    pub const TOLERANCE_FAILED: &str = "tol_fail";
}

// ── Schema table ──────────────────────────────────────────────────────────────

/// One payload shape the decoder accepts for an event. An event with several
/// shapes (such as `swap`) has one entry per arity. `fields` lists the payload
/// fields in order, as `(name, type)` pairs.
///
/// `docs/events.json` is generated from [`EVENT_SCHEMAS`]; the
/// `schema_json_matches_docs` test fails when the two drift.
pub struct EventSchema {
    pub contract: &'static str,
    pub topic: &'static str,
    pub version: u32,
    pub fields: &'static [(&'static str, &'static str)],
}

/// Every payload shape [`decode_amm_event`] recognises.
pub const EVENT_SCHEMAS: &[EventSchema] = &[
    EventSchema {
        contract: "amm",
        topic: symbols::ADD_LIQUIDITY,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[
            ("amount_a", "i128"),
            ("amount_b", "i128"),
            ("shares_minted", "i128"),
        ],
    },
    EventSchema {
        contract: "amm",
        topic: symbols::SWAP,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[
            ("token_in", "Address"),
            ("amount_in", "i128"),
            ("token_out", "Address"),
            ("amount_out", "i128"),
        ],
    },
    EventSchema {
        contract: "amm",
        topic: symbols::SWAP,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[
            ("token_in", "Address"),
            ("amount_in", "i128"),
            ("token_out", "Address"),
            ("amount_out", "i128"),
            ("referrer", "Option<Address>"),
        ],
    },
    EventSchema {
        contract: "amm",
        topic: symbols::REMOVE_LIQUIDITY,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[
            ("provider", "Address"),
            ("shares_burned", "i128"),
            ("amount_a", "i128"),
            ("amount_b", "i128"),
        ],
    },
    EventSchema {
        contract: "amm",
        topic: symbols::REMOVE_LIQUIDITY_ONE_SIDED,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[
            ("provider", "Address"),
            ("shares_burned", "i128"),
            ("token_out", "Address"),
            ("total_out", "i128"),
        ],
    },
    EventSchema {
        contract: "amm",
        topic: symbols::FLASH_LOAN,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[
            ("amount_a", "i128"),
            ("amount_b", "i128"),
            ("fee_a", "i128"),
            ("fee_b", "i128"),
        ],
    },
    EventSchema {
        contract: "amm",
        topic: symbols::FEE_UPDATED,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[("new_fee_bps", "i128")],
    },
    EventSchema {
        contract: "amm",
        topic: symbols::FLASH_FEE_UPDATED,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[("new_fee_bps", "i128")],
    },
    EventSchema {
        contract: "amm",
        topic: symbols::ADMIN_NOMINATED,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[("current_admin", "Address"), ("new_admin", "Address")],
    },
    EventSchema {
        contract: "amm",
        topic: symbols::ADMIN_CHANGED,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[("new_admin", "Address")],
    },
    EventSchema {
        contract: "amm",
        topic: symbols::UPGRADED,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[("new_wasm_hash", "BytesN<32>")],
    },
    EventSchema {
        contract: "amm",
        topic: symbols::PROTOCOL_FEE_SET,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[("protocol_fee_bps", "i128"), ("recipient", "Address")],
    },
    EventSchema {
        contract: "amm",
        topic: symbols::CIRCUIT_BREAKER,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[
            ("price_before", "i128"),
            ("price_after", "i128"),
            ("deviation_bps", "i128"),
            ("threshold_bps", "i128"),
        ],
    },
    EventSchema {
        contract: "amm",
        topic: symbols::CB_RECOVERED,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[("timestamp", "u64")],
    },
    EventSchema {
        contract: "dex_aggregator",
        topic: symbols::CL_POOL_REGISTERED,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[
            ("token_a", "Address"),
            ("token_b", "Address"),
            ("fee_bps", "i128"),
            ("pool", "Address"),
        ],
    },
    EventSchema {
        contract: "dex_aggregator",
        topic: symbols::ROUTE_SELECTED,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[
            ("venue", "Address"),
            ("venue_kind", "RouteVenueKind"),
            ("amount_in", "i128"),
            ("amount_out", "i128"),
        ],
    },
    EventSchema {
        contract: "dex_aggregator",
        topic: symbols::ROUTE_ALTERNATIVE,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[
            ("venue", "Address"),
            ("amount_out", "i128"),
            ("alt_venue", "Address"),
            ("alt_venue_kind", "RouteVenueKind"),
            ("alt_amount_out", "i128"),
        ],
    },
    EventSchema {
        contract: "dex_aggregator",
        topic: symbols::ROUTE_EXECUTED,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[
            ("trader", "Address"),
            ("token_in", "Address"),
            ("token_out", "Address"),
            ("amount_in", "i128"),
            ("amount_out", "i128"),
            ("pool", "Address"),
        ],
    },
    EventSchema {
        contract: "dex_aggregator",
        topic: symbols::TOLERANCE_FAILED,
        version: crate::EVENT_SCHEMA_VERSION,
        fields: &[
            ("pool", "Address"),
            ("observed_bps", "i128"),
            ("tolerance_bps", "i128"),
        ],
    },
];

// ── Decoder helpers ───────────────────────────────────────────────────────────

/// Wraps all possible events that can originate from an AMM pool.
#[derive(Debug, Clone, PartialEq)]
pub enum AmmEvent {
    Swap(SwapEvent),
    AddLiquidity(AddLiquidityEvent),
    RemoveLiquidity(RemoveLiquidityEvent),
    RemoveLiquidityOneSided(RemoveLiquidityOneSidedEvent),
    FlashLoan(FlashLoanEvent),
    FeeUpdated(FeeUpdatedEvent),
    FlashFeeUpdated(FlashFeeUpdatedEvent),
    AdminNominated(AdminNominatedEvent),
    AdminChanged(AdminChangedEvent),
    Upgraded(UpgradedEvent),
    ProtocolFeeSet(ProtocolFeeSetEvent),
    CircuitBreaker(CircuitBreakerEvent),
    CbRecovered(CbRecoveredEvent),
    ClPoolRegistered(ClPoolRegisteredEvent),
    RouteSelected(RouteSelectedEvent),
    RouteAlternative(RouteAlternativeEvent),
    RouteExecuted(RouteExecutedEvent),
    ToleranceFailed(ToleranceFailedEvent),
}

/// Decode a raw Soroban event `data` field given its `topics` array and `data` field.
///
/// Returns `None` if the symbol is not recognised, the schema version is mismatched,
/// or the data cannot be decoded.
///
/// # Usage
/// ```rust,ignore
/// use soroban_sdk::{Env, Vec, Val};
/// use soroban_amm_sdk::events::{decode_amm_event, AmmEvent};
///
/// // `topics` and `data` come from the RPC `getEvents` response.
/// if let Some(event) = decode_amm_event(&env, topics, raw_data_val) {
///     match event {
///         AmmEvent::Swap(e) => println!("swap: {} -> {}", e.amount_in, e.amount_out),
///         _ => {}
///     }
/// }
/// ```
///
/// In practice you would obtain the data from `stellar-sdk-rs` or the
/// Soroban RPC `getEvents` endpoint and pass the decoded XDR values here.
/// Decode an event payload into the tuple `T`, rejecting a payload that is not
/// a vector of exactly `arity` elements.
///
/// The SDK's tuple `TryFromVal` unpacks through the host's
/// `vec_unpack_to_slice`, which **traps** when the vector length does not match
/// the tuple's arity instead of returning an error — so `.ok()?` on its own
/// cannot keep `decode_amm_event` total. Event data reaching that function is
/// untrusted (it comes from RPC), so the length is checked up front and a
/// malformed payload yields `None` rather than panicking the caller.
fn decode_payload<T>(env: &soroban_sdk::Env, payload: soroban_sdk::Val, arity: u32) -> Option<T>
where
    T: soroban_sdk::TryFromVal<soroban_sdk::Env, soroban_sdk::Val>,
{
    use soroban_sdk::TryFromVal;

    let items: soroban_sdk::Vec<soroban_sdk::Val> =
        soroban_sdk::Vec::try_from_val(env, &payload).ok()?;
    if items.len() != arity {
        return None;
    }
    T::try_from_val(env, &payload).ok()
}

/// Number of elements in an event payload, or `None` if it is not a vector.
fn payload_len(env: &soroban_sdk::Env, payload: soroban_sdk::Val) -> Option<u32> {
    use soroban_sdk::TryFromVal;

    let items: soroban_sdk::Vec<soroban_sdk::Val> =
        soroban_sdk::Vec::try_from_val(env, &payload).ok()?;
    Some(items.len())
}

pub fn decode_amm_event(
    env: &soroban_sdk::Env,
    topics: soroban_sdk::Vec<soroban_sdk::Val>,
    data: soroban_sdk::Val,
) -> Option<AmmEvent> {
    use soroban_sdk::{Symbol, TryFromVal, TryIntoVal};

    if topics.is_empty() {
        return None;
    }

    let symbol_val = topics.get(0)?;
    let symbol = Symbol::try_from_val(env, &symbol_val).ok()?;

    let (version, payload_val): (u32, soroban_sdk::Val) =
        TryFromVal::try_from_val(env, &data).ok()?;
    if version != crate::EVENT_SCHEMA_VERSION {
        return None;
    }

    if symbol == Symbol::new(env, symbols::SWAP) {
        let trader: Address = topics.get(1)?.try_into_val(env).ok()?;
        // `swap` and `swap_exact_out` publish four fields. `swap_fot` also
        // publishes the optional referrer as a fifth, so the arity selects the shape.
        let (token_in, amount_in, token_out, amount_out, referrer): (
            Address,
            i128,
            Address,
            i128,
            Option<Address>,
        ) = match payload_len(env, payload_val)? {
            4 => {
                let (token_in, amount_in, token_out, amount_out): (Address, i128, Address, i128) =
                    decode_payload(env, payload_val, 4)?;
                (token_in, amount_in, token_out, amount_out, None)
            }
            5 => decode_payload(env, payload_val, 5)?,
            _ => return None,
        };
        Some(AmmEvent::Swap(SwapEvent {
            trader,
            token_in,
            amount_in,
            token_out,
            amount_out,
            referrer,
        }))
    } else if symbol == Symbol::new(env, symbols::ADD_LIQUIDITY) {
        let provider: Address = topics.get(1)?.try_into_val(env).ok()?;
        let (amount_a, amount_b, shares_minted): (i128, i128, i128) =
            decode_payload(env, payload_val, 3)?;
        Some(AmmEvent::AddLiquidity(AddLiquidityEvent {
            provider,
            amount_a,
            amount_b,
            shares_minted,
        }))
    } else if symbol == Symbol::new(env, symbols::REMOVE_LIQUIDITY) {
        let (provider, shares_burned, amount_a, amount_b): (Address, i128, i128, i128) =
            decode_payload(env, payload_val, 4)?;
        Some(AmmEvent::RemoveLiquidity(RemoveLiquidityEvent {
            provider,
            shares_burned,
            amount_a,
            amount_b,
        }))
    } else if symbol == Symbol::new(env, symbols::REMOVE_LIQUIDITY_ONE_SIDED) {
        let (provider, shares_burned, token_out, total_out): (Address, i128, Address, i128) =
            decode_payload(env, payload_val, 4)?;
        Some(AmmEvent::RemoveLiquidityOneSided(
            RemoveLiquidityOneSidedEvent {
                provider,
                shares_burned,
                token_out,
                total_out,
            },
        ))
    } else if symbol == Symbol::new(env, symbols::FLASH_LOAN) {
        let receiver: Address = topics.get(1)?.try_into_val(env).ok()?;
        let (amount_a, amount_b, fee_a, fee_b): (i128, i128, i128, i128) =
            decode_payload(env, payload_val, 4)?;
        Some(AmmEvent::FlashLoan(FlashLoanEvent {
            receiver,
            amount_a,
            amount_b,
            fee_a,
            fee_b,
        }))
    } else if symbol == Symbol::new(env, symbols::FEE_UPDATED) {
        let admin: Address = topics.get(1)?.try_into_val(env).ok()?;
        let (new_fee_bps,): (i128,) = decode_payload(env, payload_val, 1)?;
        Some(AmmEvent::FeeUpdated(FeeUpdatedEvent { admin, new_fee_bps }))
    } else if symbol == Symbol::new(env, symbols::FLASH_FEE_UPDATED) {
        let admin: Address = topics.get(1)?.try_into_val(env).ok()?;
        let (new_fee_bps,): (i128,) = decode_payload(env, payload_val, 1)?;
        Some(AmmEvent::FlashFeeUpdated(FlashFeeUpdatedEvent {
            admin,
            new_fee_bps,
        }))
    } else if symbol == Symbol::new(env, symbols::ADMIN_NOMINATED) {
        let (current_admin, new_admin): (Address, Address) = decode_payload(env, payload_val, 2)?;
        Some(AmmEvent::AdminNominated(AdminNominatedEvent {
            current_admin,
            new_admin,
        }))
    } else if symbol == Symbol::new(env, symbols::ADMIN_CHANGED) {
        let (new_admin,): (Address,) = decode_payload(env, payload_val, 1)?;
        Some(AmmEvent::AdminChanged(AdminChangedEvent { new_admin }))
    } else if symbol == Symbol::new(env, symbols::UPGRADED) {
        let (new_wasm_hash,): (BytesN<32>,) = decode_payload(env, payload_val, 1)?;
        Some(AmmEvent::Upgraded(UpgradedEvent { new_wasm_hash }))
    } else if symbol == Symbol::new(env, symbols::PROTOCOL_FEE_SET) {
        let (protocol_fee_bps, recipient): (i128, Address) = decode_payload(env, payload_val, 2)?;
        Some(AmmEvent::ProtocolFeeSet(ProtocolFeeSetEvent {
            protocol_fee_bps,
            recipient,
        }))
    } else if symbol == Symbol::new(env, symbols::CL_POOL_REGISTERED) {
        let (token_a, token_b, fee_bps, pool): (Address, Address, i128, Address) =
            decode_payload(env, payload_val, 4)?;
        Some(AmmEvent::ClPoolRegistered(ClPoolRegisteredEvent {
            token_a,
            token_b,
            fee_bps,
            pool,
        }))
    } else if symbol == Symbol::new(env, symbols::ROUTE_SELECTED) {
        let (venue, venue_kind, amount_in, amount_out): (Address, RouteVenueKind, i128, i128) =
            decode_payload(env, payload_val, 4)?;
        Some(AmmEvent::RouteSelected(RouteSelectedEvent {
            venue,
            venue_kind,
            amount_in,
            amount_out,
        }))
    } else if symbol == Symbol::new(env, symbols::ROUTE_ALTERNATIVE) {
        let (venue, amount_out, alt_venue, alt_venue_kind, alt_amount_out): (
            Address,
            i128,
            Address,
            RouteVenueKind,
            i128,
        ) = decode_payload(env, payload_val, 5)?;
        Some(AmmEvent::RouteAlternative(RouteAlternativeEvent {
            venue,
            amount_out,
            alt_venue,
            alt_venue_kind,
            alt_amount_out,
        }))
    } else if symbol == Symbol::new(env, symbols::ROUTE_EXECUTED) {
        let (trader, token_in, token_out, amount_in, amount_out, pool): (
            Address,
            Address,
            Address,
            i128,
            i128,
            Address,
        ) = decode_payload(env, payload_val, 6)?;
        Some(AmmEvent::RouteExecuted(RouteExecutedEvent {
            trader,
            token_in,
            token_out,
            amount_in,
            amount_out,
            pool,
        }))
    } else if symbol == Symbol::new(env, symbols::TOLERANCE_FAILED) {
        let (pool, observed_bps, tolerance_bps): (Address, i128, i128) =
            decode_payload(env, payload_val, 3)?;
        Some(AmmEvent::ToleranceFailed(ToleranceFailedEvent {
            pool,
            observed_bps,
            tolerance_bps,
        }))
    } else if symbol == Symbol::new(env, symbols::CIRCUIT_BREAKER) {
        let (price_before, price_after, deviation_bps, threshold_bps): (i128, i128, i128, i128) =
            decode_payload(env, payload_val, 4)?;
        Some(AmmEvent::CircuitBreaker(CircuitBreakerEvent {
            price_before,
            price_after,
            deviation_bps,
            threshold_bps,
        }))
    } else if symbol == Symbol::new(env, symbols::CB_RECOVERED) {
        let (timestamp,): (u64,) = decode_payload(env, payload_val, 1)?;
        Some(AmmEvent::CbRecovered(CbRecoveredEvent { timestamp }))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, vec, BytesN, Env, IntoVal, Symbol, Val, Vec};

    fn address(env: &Env) -> Address {
        Address::generate(env)
    }

    fn symbol(env: &Env, name: &'static str) -> Val {
        Symbol::new(env, name).into_val(env)
    }

    fn make_data(env: &Env, payload: Val, version: u32) -> Val {
        (version, payload).into_val(env)
    }

    #[test]
    fn decodes_swap() {
        let env = Env::default();
        let trader = address(&env);
        let token_in = address(&env);
        let token_out = address(&env);
        let referrer = address(&env);

        let topics = vec![
            &env,
            symbol(&env, symbols::SWAP),
            trader.clone().into_val(&env),
        ];
        let payload: Val = (
            token_in.clone(),
            1000i128,
            token_out.clone(),
            999i128,
            Some(referrer.clone()),
        )
            .into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        // Matches the `swap` event published in contracts/amm/src/lib.rs:
        // topics `(Symbol::new(&env, "swap"), trader)`, data `(EVENT_SCHEMA_VERSION, (token_in, amount_in, token_out, amount_out, referrer))`.
        assert_eq!(
            decode_amm_event(&env, topics, data),
            Some(AmmEvent::Swap(SwapEvent {
                trader,
                token_in,
                amount_in: 1000,
                token_out,
                amount_out: 999,
                referrer: Some(referrer),
            }))
        );
    }

    #[test]
    fn decodes_add_liquidity() {
        let env = Env::default();
        let provider = address(&env);

        let topics = vec![
            &env,
            symbol(&env, symbols::ADD_LIQUIDITY),
            provider.clone().into_val(&env),
        ];
        let payload: Val = (100i128, 200i128, 300i128).into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert_eq!(
            decode_amm_event(&env, topics, data),
            Some(AmmEvent::AddLiquidity(AddLiquidityEvent {
                provider,
                amount_a: 100,
                amount_b: 200,
                shares_minted: 300,
            }))
        );
    }

    #[test]
    fn decodes_remove_liquidity() {
        let env = Env::default();
        let provider = address(&env);

        let topics = vec![&env, symbol(&env, symbols::REMOVE_LIQUIDITY)];
        let payload: Val = (provider.clone(), 10i128, 11i128, 12i128).into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert_eq!(
            decode_amm_event(&env, topics, data),
            Some(AmmEvent::RemoveLiquidity(RemoveLiquidityEvent {
                provider,
                shares_burned: 10,
                amount_a: 11,
                amount_b: 12,
            }))
        );
    }

    #[test]
    fn decodes_remove_liquidity_one_sided() {
        let env = Env::default();
        let provider = address(&env);
        let token_out = address(&env);

        let topics = vec![&env, symbol(&env, symbols::REMOVE_LIQUIDITY_ONE_SIDED)];
        let payload: Val = (provider.clone(), 10i128, token_out.clone(), 120i128).into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert_eq!(
            decode_amm_event(&env, topics, data),
            Some(AmmEvent::RemoveLiquidityOneSided(
                RemoveLiquidityOneSidedEvent {
                    provider,
                    shares_burned: 10,
                    token_out,
                    total_out: 120,
                }
            ))
        );
    }

    #[test]
    fn decodes_flash_loan() {
        let env = Env::default();
        let receiver = address(&env);

        let topics = vec![
            &env,
            symbol(&env, symbols::FLASH_LOAN),
            receiver.clone().into_val(&env),
        ];
        // Matches `flash_loan` in contracts/amm/src/lib.rs: both tokens, both fees.
        let payload: Val = (10_000i128, 5_000i128, 10i128, 5i128).into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert_eq!(
            decode_amm_event(&env, topics, data),
            Some(AmmEvent::FlashLoan(FlashLoanEvent {
                receiver,
                amount_a: 10_000,
                amount_b: 5_000,
                fee_a: 10,
                fee_b: 5,
            }))
        );
    }

    #[test]
    fn decodes_swap_without_referrer() {
        let env = Env::default();
        let trader = address(&env);
        let token_in = address(&env);
        let token_out = address(&env);

        let topics = vec![
            &env,
            symbol(&env, symbols::SWAP),
            trader.clone().into_val(&env),
        ];
        // `swap` and `swap_exact_out` publish four fields and no referrer.
        let payload: Val = (token_in.clone(), 1000i128, token_out.clone(), 999i128).into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert_eq!(
            decode_amm_event(&env, topics, data),
            Some(AmmEvent::Swap(SwapEvent {
                trader,
                token_in,
                amount_in: 1000,
                token_out,
                amount_out: 999,
                referrer: None,
            }))
        );
    }

    #[test]
    fn flash_loan_with_three_fields_returns_none() {
        let env = Env::default();
        let receiver = address(&env);
        let token = address(&env);

        // The pre-fix decoder shape (token, amount, fee) is not what the AMM emits.
        let topics = vec![
            &env,
            symbol(&env, symbols::FLASH_LOAN),
            receiver.clone().into_val(&env),
        ];
        let payload: Val = (token.clone(), 10_000i128, 10i128).into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert!(decode_amm_event(&env, topics, data).is_none());
    }

    /// Each case is a payload the decoder must reject: a wrong arity, a wrong
    /// field type, a wrong schema version, or a payload that is not a vector.
    #[test]
    fn malformed_events_decode_to_none() {
        let env = Env::default();
        let who = address(&env);
        let v = crate::EVENT_SCHEMA_VERSION;

        let cases: [(&str, Val, u32); 8] = [
            // Wrong arity.
            (
                symbols::SWAP,
                (who.clone(), 1i128, who.clone()).into_val(&env),
                v,
            ),
            (symbols::FLASH_LOAN, (1i128, 2i128, 3i128).into_val(&env), v),
            (symbols::FEE_UPDATED, (1i128, 2i128).into_val(&env), v),
            // Wrong field type.
            (
                symbols::FLASH_LOAN,
                (1i128, 2i128, who.clone(), 4i128).into_val(&env),
                v,
            ),
            (symbols::CB_RECOVERED, who.clone().into_val(&env), v),
            (
                symbols::ADD_LIQUIDITY,
                (1i128, who.clone(), 3i128).into_val(&env),
                v,
            ),
            // Unknown schema version.
            (symbols::ADMIN_CHANGED, (who.clone(),).into_val(&env), v + 1),
            // Payload is not a vector at all.
            (symbols::ADMIN_CHANGED, symbol(&env, "unused"), v),
        ];

        for (topic, payload, version) in cases {
            let topics = vec![&env, symbol(&env, topic), who.clone().into_val(&env)];
            let data = make_data(&env, payload, version);
            assert!(
                decode_amm_event(&env, topics, data).is_none(),
                "{topic} should not decode"
            );
        }
    }

    #[test]
    fn swap_with_wrong_field_type_returns_none() {
        let env = Env::default();
        let trader = address(&env);
        let token_in = address(&env);
        let token_out = address(&env);

        let topics = vec![
            &env,
            symbol(&env, symbols::SWAP),
            trader.clone().into_val(&env),
        ];
        // amount_in is an address, not an i128.
        let payload: Val = (
            token_in.clone(),
            token_in.clone(),
            token_out.clone(),
            999i128,
        )
            .into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert!(decode_amm_event(&env, topics, data).is_none());
    }

    #[test]
    fn decodes_fee_updated() {
        let env = Env::default();
        let admin = address(&env);

        let topics = vec![
            &env,
            symbol(&env, symbols::FEE_UPDATED),
            admin.clone().into_val(&env),
        ];
        let payload: Val = (25i128,).into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert_eq!(
            decode_amm_event(&env, topics, data),
            Some(AmmEvent::FeeUpdated(FeeUpdatedEvent {
                admin,
                new_fee_bps: 25,
            }))
        );
    }

    #[test]
    fn decodes_flash_fee_updated() {
        let env = Env::default();
        let admin = address(&env);

        let topics = vec![
            &env,
            symbol(&env, symbols::FLASH_FEE_UPDATED),
            admin.clone().into_val(&env),
        ];
        let payload: Val = (35i128,).into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert_eq!(
            decode_amm_event(&env, topics, data),
            Some(AmmEvent::FlashFeeUpdated(FlashFeeUpdatedEvent {
                admin,
                new_fee_bps: 35,
            }))
        );
    }

    #[test]
    fn decodes_admin_nominated() {
        let env = Env::default();
        let current_admin = address(&env);
        let new_admin = address(&env);

        let topics = vec![&env, symbol(&env, symbols::ADMIN_NOMINATED)];
        let payload: Val = (current_admin.clone(), new_admin.clone()).into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert_eq!(
            decode_amm_event(&env, topics, data),
            Some(AmmEvent::AdminNominated(AdminNominatedEvent {
                current_admin,
                new_admin,
            }))
        );
    }

    #[test]
    fn decodes_admin_changed() {
        let env = Env::default();
        let new_admin = address(&env);

        let topics = vec![&env, symbol(&env, symbols::ADMIN_CHANGED)];
        let payload: Val = (new_admin.clone(),).into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert_eq!(
            decode_amm_event(&env, topics, data),
            Some(AmmEvent::AdminChanged(AdminChangedEvent { new_admin }))
        );
    }

    #[test]
    fn decodes_upgraded() {
        let env = Env::default();
        let new_wasm_hash: BytesN<32> = BytesN::from_array(&env, &[7u8; 32]);

        let topics = vec![&env, symbol(&env, symbols::UPGRADED)];
        let payload: Val = (new_wasm_hash.clone(),).into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert_eq!(
            decode_amm_event(&env, topics, data),
            Some(AmmEvent::Upgraded(UpgradedEvent { new_wasm_hash }))
        );
    }

    #[test]
    fn decodes_protocol_fee_set() {
        let env = Env::default();
        let recipient = address(&env);

        let topics = vec![&env, symbol(&env, symbols::PROTOCOL_FEE_SET)];
        let payload: Val = (500i128, recipient.clone()).into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert_eq!(
            decode_amm_event(&env, topics, data),
            Some(AmmEvent::ProtocolFeeSet(ProtocolFeeSetEvent {
                protocol_fee_bps: 500,
                recipient,
            }))
        );
    }

    #[test]
    fn decodes_circuit_breaker() {
        let env = Env::default();

        let topics = vec![&env, symbol(&env, symbols::CIRCUIT_BREAKER)];
        let payload: Val = (1_000_000i128, 1_500_000i128, 400i128, 300i128).into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        // Matches the `circuit_break` event published in contracts/amm/src/lib.rs:
        // data `(EVENT_SCHEMA_VERSION, (price_before, price_after, deviation_bps, threshold_bps))`.
        assert_eq!(
            decode_amm_event(&env, topics, data),
            Some(AmmEvent::CircuitBreaker(CircuitBreakerEvent {
                price_before: 1_000_000,
                price_after: 1_500_000,
                deviation_bps: 400,
                threshold_bps: 300,
            }))
        );
    }

    #[test]
    fn decodes_cb_recovered() {
        let env = Env::default();

        let topics = vec![&env, symbol(&env, symbols::CB_RECOVERED)];
        let payload: Val = (1_700_000_000u64,).into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert_eq!(
            decode_amm_event(&env, topics, data),
            Some(AmmEvent::CbRecovered(CbRecoveredEvent {
                timestamp: 1_700_000_000
            }))
        );
    }

    #[test]
    fn empty_topics_returns_none() {
        let env = Env::default();
        let topics: Vec<Val> = Vec::new(&env);
        let payload: Val = symbol(&env, "unused");
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert!(decode_amm_event(&env, topics, data).is_none());
    }

    #[test]
    fn wrong_schema_version_returns_none() {
        let env = Env::default();
        let trader = address(&env);

        let topics = vec![
            &env,
            symbol(&env, symbols::SWAP),
            trader.clone().into_val(&env),
        ];
        let payload: Val = (
            trader.clone(),
            100i128,
            trader.clone(),
            99i128,
            None::<Address>,
        )
            .into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION + 1);

        assert!(decode_amm_event(&env, topics, data).is_none());
    }

    #[test]
    fn unrecognized_symbol_returns_none() {
        let env = Env::default();
        let topics = vec![&env, symbol(&env, "not_an_amm_event")];
        let payload: Val = symbol(&env, "unused");
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert!(decode_amm_event(&env, topics, data).is_none());
    }

    #[test]
    fn swap_payload_arity_mismatch_returns_none() {
        let env = Env::default();
        let trader = address(&env);
        let token_in = address(&env);

        let topics = vec![
            &env,
            symbol(&env, symbols::SWAP),
            trader.clone().into_val(&env),
        ];
        let payload: Val = (token_in.clone(), 100i128).into_val(&env);
        let data = make_data(&env, payload, crate::EVENT_SCHEMA_VERSION);

        assert!(decode_amm_event(&env, topics, data).is_none());
    }
}
