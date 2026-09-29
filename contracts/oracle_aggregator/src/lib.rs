#![no_std]
//! Oracle aggregator: reports the **weighted-median** price over fresh,
//! agreeing sources.
//!
//! Each source carries a configurable `weight` (basis-point style,
//! 10_000 = 1.0×). A quote contributes to the result only when it is both
//! fresh (reported within `max_staleness_seconds`) and *agreeing* — within
//! a configurable deviation band (in basis points) of the **unweighted**
//! reference median. The reference median stays unweighted so a single heavy
//! source cannot drag the deviation band around itself.
//!
//! Sources outside the band are dropped and reported via a `deviant` event.
//! The final price is the **weighted median** of the agreeing subset, and
//! `confidence` is the summed weight of agreeing sources (not a raw count).
//! A minimum total agreeing weight floor ([`MIN_AGREEING_WEIGHT`]) prevents
//! returning a price backed by too little weight.
//!
//! The band defaults to [`DEFAULT_MAX_DEVIATION_BPS`] and is tunable by the
//! admin via [`OracleAggregator::set_max_deviation_bps`].

use soroban_amm_sdk::emit_versioned_event;
use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, panic_with_error,
    symbol_short, Address, Env, Vec,
};

// ── Public types ────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OracleSourceType {
    AmmTwap = 0,
    ClTwap = 1,
    External = 2,
}

#[contracttype]
#[derive(Clone)]
pub struct OracleSource {
    pub source_contract: Address,
    pub source_type: OracleSourceType,
    /// Last timestamp reported by the source itself
    pub last_updated_at: u64,
    /// Weight in basis points (10_000 = 1.0×).
    pub weight: u32,
}

#[contracttype]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuoteStatus {
    Agreed = 0,
    Deviant = 1,
    Stale = 2,
    NoQuote = 3,
}

/// A single source's contribution to an aggregated price.
#[contracttype]
#[derive(Clone)]
pub struct SourceQuote {
    pub source: Address,
    pub price: i128,
    pub timestamp: u64,
    pub weight: u32,
    pub status: QuoteStatus,
}

/// Full result of an aggregation, including per-source breakdown.
#[contracttype]
#[derive(Clone)]
pub struct PriceBreakdown {
    pub price: i128,
    pub confidence: u32,
    pub source_quotes: Vec<SourceQuote>,
}

#[contracttype]
#[derive(Clone)]
pub struct AggregatedPrice {
    pub price: i128,
    pub confidence: u32,
}

#[contracterror]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum OracleError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    NotAdmin = 3,
    SourceAlreadyRegistered = 4,
    SourceNotFound = 5,
    InsufficientSources = 6,
    InvalidStaleness = 7,
    InvalidDeviation = 8,
    InvalidWeight = 9,
    WeightFloorNotMet = 10,
    NoPendingAdmin = 11,
    WrongAdmin = 12,
    Paused = 13,
}

// ── Storage ────────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Admin,
    PendingAdmin,
    MaxStaleness,
    Sources,
    MaxDeviationBps,
    Paused,
}

pub const MIN_VALID_SOURCES: u32 = 2;

/// Basis-points denominator (100% = 10_000 bps).
pub const BPS_DENOMINATOR: i128 = 10_000;

/// Default deviation band: a fresh quote must be within 5% of the median of
/// the fresh quotes to count as agreeing. Chosen tight enough to flag the
/// "pull the price halfway to the attacker" manipulation described in the
/// aggregator's threat model, while tolerating normal cross-venue spread.
pub const DEFAULT_MAX_DEVIATION_BPS: u32 = 500;

/// Default source weight (1.0×).
pub const DEFAULT_SOURCE_WEIGHT: u32 = 10_000;

/// Maximum weight a single source can hold (10.0×).
pub const MAX_SOURCE_WEIGHT: u32 = 100_000;

/// Minimum total agreeing weight required to return a confident price.
/// Default requires at least 2 default-weight sources to agree.
pub const MIN_AGREEING_WEIGHT: u32 = 20_000;

// ── Adapter client (UPDATED) ───────────────────────────────────────────────

#[contractclient(name = "OracleSourceAdapterClient")]
pub trait OracleSourceAdapter {
    /// Returns (price, last_updated_timestamp)
    fn quote(env: Env, token_a: Address, token_b: Address) -> (i128, u64);
}

// ── Contract ───────────────────────────────────────────────────────────────

#[contract]
pub struct OracleAggregator;

#[contractimpl]
impl OracleAggregator {
    pub fn initialize(env: Env, admin: Address, max_staleness_seconds: u64) {
        Self::extend_ttl(&env);
        if env.storage().instance().has(&DataKey::Admin) {
            panic_with_error!(&env, OracleError::AlreadyInitialized);
        }
        if max_staleness_seconds == 0 {
            panic_with_error!(&env, OracleError::InvalidStaleness);
        }
        admin.require_auth();

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::MaxStaleness, &max_staleness_seconds);
        env.storage()
            .instance()
            .set(&DataKey::MaxDeviationBps, &DEFAULT_MAX_DEVIATION_BPS);

        let empty: Vec<OracleSource> = Vec::new(&env);
        env.storage().instance().set(&DataKey::Sources, &empty);
    }

    pub fn propose_admin(env: Env, current_admin: Address, new_admin: Address) {
        require_admin(&env, &current_admin);
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (soroban_sdk::Symbol::new(&env, "admin_nominated"),),
            (current_admin, new_admin)
        );
    }

    pub fn accept_admin(env: Env, new_admin: Address) {
        let pending: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None);
        let nominee =
            pending.unwrap_or_else(|| panic_with_error!(&env, OracleError::NoPendingAdmin));
        if new_admin != nominee {
            panic_with_error!(&env, OracleError::WrongAdmin);
        }
        new_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (soroban_sdk::Symbol::new(&env, "admin_changed"),),
            (new_admin,)
        );
    }

    pub fn pause(env: Env, admin: Address) {
        require_admin(&env, &admin);
        env.storage().instance().set(&DataKey::Paused, &true);
        env.events().publish((symbol_short!("pause"),), ());
    }

    pub fn unpause(env: Env, admin: Address) {
        require_admin(&env, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.events().publish((symbol_short!("unpause"),), ());
    }

    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    pub fn register_source(
        env: Env,
        admin: Address,
        source_contract: Address,
        source_type: OracleSourceType,
        weight: u32,
    ) {
        Self::extend_ttl(&env);
        require_admin(&env, &admin);

        if weight == 0 || weight > MAX_SOURCE_WEIGHT {
            panic_with_error!(&env, OracleError::InvalidWeight);
        }

        let mut sources = read_sources(&env);
        for i in 0..sources.len() {
            if sources.get_unchecked(i).source_contract == source_contract {
                panic_with_error!(&env, OracleError::SourceAlreadyRegistered);
            }
        }

        sources.push_back(OracleSource {
            source_contract: source_contract.clone(),
            source_type,
            last_updated_at: 0,
            weight,
        });

        env.storage().instance().set(&DataKey::Sources, &sources);
    }

    pub fn remove_source(env: Env, admin: Address, source_contract: Address) {
        Self::extend_ttl(&env);
        require_admin(&env, &admin);

        let sources = read_sources(&env);
        let mut new_sources: Vec<OracleSource> = Vec::new(&env);
        let mut found = false;
        for i in 0..sources.len() {
            let source = sources.get_unchecked(i);
            if source.source_contract == source_contract {
                found = true;
            } else {
                new_sources.push_back(source);
            }
        }

        if !found {
            panic_with_error!(&env, OracleError::SourceNotFound);
        }

        if new_sources.is_empty() {
            panic_with_error!(&env, OracleError::InsufficientSources);
        }

        env.storage()
            .instance()
            .set(&DataKey::Sources, &new_sources);
    }

    pub fn get_price(env: Env, token_a: Address, token_b: Address) -> AggregatedPrice {
        Self::extend_ttl(&env);
        if Self::is_paused(env.clone()) {
            panic_with_error!(&env, OracleError::Paused);
        }
        let breakdown = Self::aggregate_price(&env, token_a.clone(), token_b.clone(), true);
        if breakdown.confidence == 0 {
            panic_with_error!(&env, OracleError::InsufficientSources);
        }

        emit_versioned_event!(
            env,
            (symbol_short!("price"),),
            (token_a, token_b, breakdown.price, breakdown.confidence)
        );

        AggregatedPrice {
            price: breakdown.price,
            confidence: breakdown.confidence,
        }
    }

    pub fn get_price_safe(env: Env, token_a: Address, token_b: Address) -> AggregatedPrice {
        Self::extend_ttl(&env);
        let breakdown = Self::aggregate_price(&env, token_a, token_b, false);
        AggregatedPrice {
            price: breakdown.price,
            confidence: breakdown.confidence,
        }
    }

    /// Returns the full aggregation breakdown including per-source status.
    /// Read-only: does not persist source timestamps.
    pub fn get_price_detailed(
        env: Env,
        token_a: Address,
        token_b: Address,
    ) -> (AggregatedPrice, Vec<SourceQuote>) {
        Self::extend_ttl(&env);
        let breakdown = Self::aggregate_price(&env, token_a, token_b, false);
        let price = AggregatedPrice {
            price: breakdown.price,
            confidence: breakdown.confidence,
        };
        (price, breakdown.source_quotes)
    }

    /// Spread between the highest and lowest agreeing quote, in basis
    /// points. Returns 0 when fewer than two sources agree.
    pub fn get_price_spread_bps(env: Env, token_a: Address, token_b: Address) -> u32 {
        Self::extend_ttl(&env);
        let breakdown = Self::aggregate_price(&env, token_a, token_b, false);
        let mut min_price: i128 = 0;
        let mut max_price: i128 = 0;
        let mut found = false;
        for i in 0..breakdown.source_quotes.len() {
            let sq = breakdown.source_quotes.get_unchecked(i);
            if matches!(sq.status, QuoteStatus::Agreed) {
                if !found {
                    min_price = sq.price;
                    max_price = sq.price;
                    found = true;
                } else {
                    if sq.price < min_price {
                        min_price = sq.price;
                    }
                    if sq.price > max_price {
                        max_price = sq.price;
                    }
                }
            }
        }
        if !found || min_price == 0 {
            return 0;
        }
        ((max_price - min_price).saturating_mul(BPS_DENOMINATOR) / min_price) as u32
    }

    /// Admin: update the weight of a registered source.
    /// Emits a `src_wt` event with the old and new weight.
    pub fn set_source_weight(env: Env, admin: Address, source_contract: Address, weight: u32) {
        Self::extend_ttl(&env);
        require_admin(&env, &admin);

        if weight == 0 || weight > MAX_SOURCE_WEIGHT {
            panic_with_error!(&env, OracleError::InvalidWeight);
        }

        let mut sources = read_sources(&env);
        let mut found = false;
        for i in 0..sources.len() {
            let mut source = sources.get_unchecked(i);
            if source.source_contract == source_contract {
                let old_weight = source.weight;
                source.weight = weight;
                sources.set(i, source);
                found = true;

                emit_versioned_event!(
                    env,
                    (symbol_short!("src_wt"),),
                    (source_contract, old_weight, weight)
                );
                break;
            }
        }

        if !found {
            panic_with_error!(&env, OracleError::SourceNotFound);
        }

        env.storage().instance().set(&DataKey::Sources, &sources);
    }

    /// Admin-only migration: rewrites all stored source records, defaulting
    /// any pre-upgrade record missing a `weight` field to
    /// `DEFAULT_SOURCE_WEIGHT` (10_000 = 1.0×).
    pub fn migrate_sources(env: Env, admin: Address) {
        Self::extend_ttl(&env);
        require_admin(&env, &admin);
        let mut sources = read_sources(&env);
        let mut migrated = false;
        for i in 0..sources.len() {
            let mut source = sources.get_unchecked(i);
            if source.weight == 0 {
                source.weight = DEFAULT_SOURCE_WEIGHT;
                sources.set(i, source);
                migrated = true;
            }
        }
        if migrated {
            env.storage().instance().set(&DataKey::Sources, &sources);
        }
    }

    fn aggregate_price(
        env: &Env,
        token_a: Address,
        token_b: Address,
        persist_sources: bool,
    ) -> PriceBreakdown {
        let sources = read_sources(env);

        if sources.is_empty() {
            return PriceBreakdown {
                price: 0,
                confidence: 0,
                source_quotes: Vec::new(env),
            };
        }

        let now = env.ledger().timestamp();
        let max_staleness: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MaxStaleness)
            .unwrap_or(0);

        let mut prices: Vec<i128> = Vec::new(env);
        let mut weights: Vec<u32> = Vec::new(env);
        let mut fresh_sources: Vec<Address> = Vec::new(env);
        let mut updated: Vec<OracleSource> = Vec::new(env);
        let mut stale_sources: Vec<Address> = Vec::new(env);
        let mut all_quotes: Vec<(Address, i128, u64, u32)> = Vec::new(env);

        for i in 0..sources.len() {
            let mut source = sources.get_unchecked(i);
            let src_addr = source.source_contract.clone();
            let w = source.weight;

            let client = OracleSourceAdapterClient::new(env, &source.source_contract);

            let (price, source_timestamp) = match client.try_quote(&token_a, &token_b) {
                Ok(Ok(res)) => res,
                _ => (0, 0),
            };

            let is_fresh = source_timestamp > 0
                && source_timestamp <= now
                && now - source_timestamp <= max_staleness;

            let mut contributed = false;

            if price > 0 && is_fresh {
                source.last_updated_at = source_timestamp;
                prices.push_back(price);
                weights.push_back(w);
                fresh_sources.push_back(src_addr.clone());
                contributed = true;
            }

            if !contributed {
                stale_sources.push_back(src_addr.clone());
            }

            all_quotes.push_back((src_addr, price, source_timestamp, w));
            updated.push_back(source);
        }

        if !stale_sources.is_empty() {
            emit_versioned_event!(env, (symbol_short!("stale_src"),), (stale_sources,));
        }

        if prices.len() < MIN_VALID_SOURCES {
            return PriceBreakdown {
                price: 0,
                confidence: 0,
                source_quotes: Self::build_source_quotes(
                    env,
                    &all_quotes,
                    &Vec::new(env),
                    &Vec::new(env),
                ),
            };
        }

        if persist_sources {
            env.storage().instance().set(&DataKey::Sources, &updated);
        }

        // Reference median remains unweighted: the outlier test should not
        // be swayed by weights, or a heavy source could drag the band.
        let reference_median = median_i128(env, &prices);
        let max_deviation_bps = read_max_deviation_bps(env);

        let mut agreeing: Vec<(i128, u32)> = Vec::new(env);
        let mut deviant_sources: Vec<Address> = Vec::new(env);
        for i in 0..prices.len() {
            let price = prices.get_unchecked(i);
            if within_deviation_band(price, reference_median, max_deviation_bps) {
                agreeing.push_back((price, weights.get_unchecked(i)));
            } else {
                deviant_sources.push_back(fresh_sources.get_unchecked(i));
            }
        }

        if !deviant_sources.is_empty() {
            emit_versioned_event!(env, (symbol_short!("deviant"),), (deviant_sources.clone(),));
        }

        // Check minimum total agreeing weight.
        let total_weight: u32 = agreeing.iter().map(|(_, w)| w).sum();
        if total_weight < MIN_AGREEING_WEIGHT {
            return PriceBreakdown {
                price: 0,
                confidence: 0,
                source_quotes: Self::build_source_quotes(
                    env,
                    &all_quotes,
                    &agreeing,
                    &deviant_sources,
                ),
            };
        }

        let median = weighted_median_i128(env, &agreeing);

        PriceBreakdown {
            price: median,
            confidence: total_weight,
            source_quotes: Self::build_source_quotes(env, &all_quotes, &agreeing, &deviant_sources),
        }
    }

    /// Classify each source quote and build the per-source breakdown.
    fn build_source_quotes(
        env: &Env,
        all_quotes: &Vec<(Address, i128, u64, u32)>,
        agreeing: &Vec<(i128, u32)>,
        deviant_addrs: &Vec<Address>,
    ) -> Vec<SourceQuote> {
        let mut result: Vec<SourceQuote> = Vec::new(env);
        for i in 0..all_quotes.len() {
            let (addr, price, ts, w) = all_quotes.get_unchecked(i);
            let status = if price == 0 {
                QuoteStatus::NoQuote
            } else if deviant_addrs.contains(&addr) {
                QuoteStatus::Deviant
            } else if Self::is_in_agreeing(price, w, agreeing) {
                QuoteStatus::Agreed
            } else {
                QuoteStatus::Stale
            };
            result.push_back(SourceQuote {
                source: addr,
                price,
                timestamp: ts,
                weight: w,
                status,
            });
        }
        result
    }

    fn is_in_agreeing(price: i128, _weight: u32, agree_set: &Vec<(i128, u32)>) -> bool {
        for i in 0..agree_set.len() {
            let (p, _w) = agree_set.get_unchecked(i);
            if p == price {
                return true;
            }
        }
        false
    }

    pub fn list_sources(env: Env) -> Vec<OracleSource> {
        Self::extend_ttl(&env);
        read_sources(&env)
    }

    pub fn get_max_staleness(env: Env) -> u64 {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::MaxStaleness)
            .unwrap_or(0)
    }

    pub fn set_max_staleness(env: Env, admin: Address, max_staleness_seconds: u64) {
        Self::extend_ttl(&env);
        require_admin(&env, &admin);
        if max_staleness_seconds == 0 {
            panic_with_error!(&env, OracleError::InvalidStaleness);
        }
        env.storage()
            .instance()
            .set(&DataKey::MaxStaleness, &max_staleness_seconds);
    }

    /// Returns the deviation band (in bps) a fresh quote must fall within of
    /// the median to be counted as agreeing.
    pub fn get_max_deviation_bps(env: Env) -> u32 {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::MaxDeviationBps)
            .unwrap_or(DEFAULT_MAX_DEVIATION_BPS)
    }

    /// Admin: set the agreement deviation band, in bps (must be `1..=10_000`).
    pub fn set_max_deviation_bps(env: Env, admin: Address, max_deviation_bps: u32) {
        Self::extend_ttl(&env);
        require_admin(&env, &admin);
        if max_deviation_bps == 0 || max_deviation_bps as i128 > BPS_DENOMINATOR {
            panic_with_error!(&env, OracleError::InvalidDeviation);
        }
        env.storage()
            .instance()
            .set(&DataKey::MaxDeviationBps, &max_deviation_bps);
    }

    pub fn get_admin(env: Env) -> Address {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, OracleError::NotInitialized))
    }

    /// Instance-storage TTL maintenance.
    ///
    /// The oracle aggregator keeps every entry (`Admin`, `MaxStaleness`,
    /// `Sources`, `MaxDeviationBps`) in instance storage, so the instance entry
    /// holds all of the contract's state as well as its executable. If that
    /// entry's TTL lapses the contract is archived and every call traps until
    /// someone restores it — and downstream pools reading this for price sanity
    /// checks trap with it. Every public entrypoint therefore bumps the
    /// instance TTL, including the read-only price and getter paths, which are
    /// the only traffic a sporadically-read oracle may see for long stretches.
    ///
    /// The threshold and bump are in ledgers. At ~5 s per ledger, 172_800
    /// ledgers is about 10 days (top up when less than this remains) and
    /// 518_400 ledgers is about 30 days (the target live-until). These match
    /// the `amm` reference contract so the whole workspace ages its instance
    /// entries on one schedule.
    fn extend_ttl(env: &Env) {
        env.storage().instance().extend_ttl(172_800, 518_400);
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn read_sources(env: &Env) -> Vec<OracleSource> {
    env.storage()
        .instance()
        .get(&DataKey::Sources)
        .unwrap_or_else(|| panic_with_error!(env, OracleError::NotInitialized))
}

fn require_admin(env: &Env, claimed: &Address) {
    let admin: Address = env
        .storage()
        .instance()
        .get(&DataKey::Admin)
        .unwrap_or_else(|| panic_with_error!(env, OracleError::NotInitialized));

    if &admin != claimed {
        panic_with_error!(env, OracleError::NotAdmin);
    }

    claimed.require_auth();
}

fn read_max_deviation_bps(env: &Env) -> i128 {
    let bps: u32 = env
        .storage()
        .instance()
        .get(&DataKey::MaxDeviationBps)
        .unwrap_or(DEFAULT_MAX_DEVIATION_BPS);
    bps as i128
}

/// Returns `true` when `price` is within `max_deviation_bps` of `median`.
///
/// Uses the cross-multiplied form `|price - median| * 10_000 <= bps * median`
/// to avoid a division (and its precision loss). `median` is always `> 0`
/// here because it is derived from strictly-positive quotes.
fn within_deviation_band(price: i128, median: i128, max_deviation_bps: i128) -> bool {
    let diff = price - median;
    let abs_diff = if diff < 0 { -diff } else { diff };
    abs_diff.saturating_mul(BPS_DENOMINATOR) <= max_deviation_bps.saturating_mul(median)
}

fn median_i128(env: &Env, values: &Vec<i128>) -> i128 {
    let n = values.len();
    let mut sorted: Vec<i128> = Vec::new(env);

    for i in 0..n {
        let v = values.get_unchecked(i);
        let mut inserted = false;
        let mut next: Vec<i128> = Vec::new(env);

        for j in 0..sorted.len() {
            let s = sorted.get_unchecked(j);
            if !inserted && v < s {
                next.push_back(v);
                inserted = true;
            }
            next.push_back(s);
        }

        if !inserted {
            next.push_back(v);
        }

        sorted = next;
    }

    let mid = sorted.len() / 2;

    if sorted.len().is_multiple_of(2) {
        let lo = sorted.get_unchecked(mid - 1);
        let hi = sorted.get_unchecked(mid);
        // Midpoint via `lo + (hi - lo) / 2` rather than `(lo + hi) / 2`.
        // The sum of two individually-valid prices can exceed i128::MAX and
        // overflow (panic in debug, wraparound in release). Since `sorted` is
        // ascending, `hi >= lo`, and aggregated prices are strictly positive,
        // so `hi - lo` stays within range and the midpoint never overflows.
        lo + (hi - lo) / 2
    } else {
        sorted.get_unchecked(mid)
    }
}

/// Weighted median: sort `(price, weight)` pairs by price, accumulate
/// weight, and return the price at which the running total first reaches
/// half the total weight.
fn weighted_median_i128(env: &Env, pairs: &Vec<(i128, u32)>) -> i128 {
    if pairs.is_empty() {
        return 0;
    }

    let total_weight: i128 = pairs.iter().map(|(_, w)| w as i128).sum();
    if total_weight == 0 {
        return 0;
    }

    let half = total_weight / 2;

    // Insertion sort by price.
    let mut sorted: Vec<(i128, u32)> = Vec::new(env);
    for i in 0..pairs.len() {
        let (p, w) = pairs.get_unchecked(i);
        let mut inserted = false;
        let mut next: Vec<(i128, u32)> = Vec::new(env);
        for j in 0..sorted.len() {
            let (sp, sw) = sorted.get_unchecked(j);
            if !inserted && p < sp {
                next.push_back((p, w));
                inserted = true;
            }
            next.push_back((sp, sw));
        }
        if !inserted {
            next.push_back((p, w));
        }
        sorted = next;
    }

    let mut acc: i128 = 0;
    for i in 0..sorted.len() {
        let (p, w) = sorted.get_unchecked(i);
        acc += w as i128;
        if acc > half {
            return p;
        }
        // When cumulative weight exactly equals half (even-count, equal
        // weights), average with the next element to match the unweighted
        // median midpoint. Without this, equal-weight pairs return the
        // higher price instead of the midpoint.
        if acc == half && i + 1 < sorted.len() {
            let (next_p, _next_w) = sorted.get_unchecked(i + 1);
            let diff = next_p - p;
            return p + diff / 2;
        }
    }

    // Fallback: return last price.
    sorted.get_unchecked(sorted.len() - 1).0
}

#[cfg(test)]
mod test;
