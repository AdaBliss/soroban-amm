//! Concentrated Liquidity pool model.
//!
//! Mirrors the on-chain contracts/concentrated_liquidity contract.
//! Implements the state and operations needed to simulate CL pool behavior:
//! pricing the pool, adding liquidity to a tick range, and swapping across
//! initialized ticks (see [`super::swap`]).

use super::swap_math::{self, MAX_TICK, MIN_TICK};
use crate::error::{Result, SimulationError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Fixed-point scale of the fee-growth accumulators, as in the contract
/// (`lp_fee * 1_000_000 / active_liquidity`).
pub const FEE_GROWTH_SCALE: i128 = 1_000_000;

/// Tick structure holding liquidity and fee growth data.
///
/// Mirrors the contract's `TickInfo`. A tick is present in
/// [`ClPoolState::ticks`] only while some position references it
/// (`liquidity_gross > 0`); the contract deletes the entry otherwise.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Tick {
    /// Total liquidity of the positions that use this tick as a boundary.
    #[serde(default)]
    pub liquidity_gross: i128,
    /// Net liquidity added when this tick is crossed upward (subtracted when
    /// crossed downward).
    pub liquidity_net: i128,
    /// Token A fee growth on the side of this tick away from the current tick.
    pub fee_growth_outside_a: i128,
    /// Token B fee growth on the side of this tick away from the current tick.
    pub fee_growth_outside_b: i128,
}

/// Position held by an LP in a concentrated liquidity pool.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Position {
    /// Owner address
    pub owner: String,
    /// Lower tick (inclusive)
    pub lower_tick: i32,
    /// Upper tick (exclusive)
    pub upper_tick: i32,
    /// Liquidity in this position
    pub liquidity: i128,
    /// Fee growth inside the range when fees were last settled
    pub fee_growth_inside_a_snapshot: i128,
    pub fee_growth_inside_b_snapshot: i128,
    /// Fees settled into the position but not yet collected
    #[serde(default)]
    pub tokens_owed_a: i128,
    #[serde(default)]
    pub tokens_owed_b: i128,
}

/// State of a concentrated liquidity pool.
///
/// The sqrt price and the current tick are a coupled pair — the tick is
/// always the largest one whose price is at or below the sqrt price — so they
/// are only readable through [`Self::sqrt_price_x96`] and
/// [`Self::current_tick`] and only change through [`Self::initialize`],
/// [`Self::initialize_at_tick`] and [`Self::swap`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ClPoolState {
    /// Tokens in the pool
    pub token_a: String,
    pub token_b: String,

    /// Current price in Q64.96 fixed-point format
    pub(super) sqrt_price_x96: i128,

    /// Current active tick
    pub(super) current_tick: i32,

    /// Total liquidity in the active tick range
    pub liquidity: i128,

    /// Global fee growth accumulated in token A (per unit liquidity,
    /// scaled by [`FEE_GROWTH_SCALE`])
    pub fee_growth_global_a: i128,
    /// Global fee growth accumulated in token B (per unit liquidity,
    /// scaled by [`FEE_GROWTH_SCALE`])
    pub fee_growth_global_b: i128,

    /// Tick spacing (e.g., 1 tick per step = tick_spacing of 1)
    pub tick_spacing: i32,

    /// Fee in basis points (e.g., 30 = 0.3%)
    pub fee_bps: i128,

    /// Share of each swap fee that goes to the protocol, in basis points
    #[serde(default)]
    pub protocol_fee_bps: i128,

    /// Cumulative LP swap fees, by input token
    pub accrued_fee_a: i128,
    pub accrued_fee_b: i128,

    /// Protocol fee pool
    pub protocol_fee_a: i128,
    pub protocol_fee_b: i128,

    /// Map of initialized ticks to their state
    pub ticks: BTreeMap<i32, Tick>,

    /// Positions by ID (owner + lower + upper tick uniquely identify)
    pub positions: BTreeMap<String, Position>,

    /// Last timestamp for TWAP calculation
    pub last_timestamp: u64,

    /// Paused flag
    pub paused: bool,
}

impl ClPoolState {
    /// Create a new concentrated liquidity pool, priced at tick 0 (price 1.0).
    pub fn new(
        token_a: impl Into<String>,
        token_b: impl Into<String>,
        fee_bps: i128,
        tick_spacing: i32,
    ) -> Result<Self> {
        let pool = Self {
            token_a: token_a.into(),
            token_b: token_b.into(),
            sqrt_price_x96: swap_math::tick_to_sqrt_price_x96(0) as i128,
            current_tick: 0,
            liquidity: 0,
            fee_growth_global_a: 0,
            fee_growth_global_b: 0,
            tick_spacing,
            fee_bps,
            protocol_fee_bps: 0,
            accrued_fee_a: 0,
            accrued_fee_b: 0,
            protocol_fee_a: 0,
            protocol_fee_b: 0,
            ticks: BTreeMap::new(),
            positions: BTreeMap::new(),
            last_timestamp: 0,
            paused: false,
        };
        pool.validate()?;
        Ok(pool)
    }

    /// Validate the pool state.
    pub fn validate(&self) -> Result<()> {
        if self.token_a == self.token_b {
            return Err(SimulationError::InvalidToken {
                token: self.token_a.clone(),
            });
        }
        // The contract rejects a 100% fee (`!(0..10_000).contains`): its swap
        // grosses up the after-fee input by dividing by `10_000 - fee_bps`.
        if !(0..10_000).contains(&self.fee_bps) {
            return Err(SimulationError::InvalidFeeBps {
                fee_bps: self.fee_bps,
            });
        }
        if !(0..=10_000).contains(&self.protocol_fee_bps) {
            return Err(SimulationError::InvalidFeeBps {
                fee_bps: self.protocol_fee_bps,
            });
        }
        if self.tick_spacing <= 0 || self.tick_spacing > 32767 {
            return Err(SimulationError::InvalidTickSpacing {
                tick_spacing: self.tick_spacing,
            });
        }
        Ok(())
    }

    /// Check if the pool is empty (no liquidity).
    pub fn is_empty(&self) -> bool {
        self.liquidity == 0
    }

    /// Current sqrt price in Q64.96.
    pub fn sqrt_price_x96(&self) -> i128 {
        self.sqrt_price_x96
    }

    /// Current tick: the largest tick whose sqrt price is at or below
    /// [`Self::sqrt_price_x96`], using the contract's swap-path mapping
    /// ([`swap_math::sqrt_price_x96_to_tick`]).
    pub fn current_tick(&self) -> i32 {
        self.current_tick
    }

    /// Get the spot price of token B in terms of token A.
    ///
    /// Formula: spot_price = (sqrt_price_x96 / 2^96)^2
    pub fn spot_price_b(&self) -> f64 {
        let price_f = self.sqrt_price_x96 as f64 / ((1_u128 << 96) as f64);
        price_f * price_f
    }

    /// Initialize the pool with a starting price, deriving the current tick
    /// from it.
    ///
    /// The tick comes from the same price mapping the contract's swap uses,
    /// so a pool priced here and one the contract prices from the resulting
    /// tick step through ticks identically. The price must lie within the
    /// range that mapping can represent.
    ///
    /// Re-pricing is refused once the pool holds liquidity: active liquidity
    /// and the fee-growth-outside values are only meaningful relative to the
    /// tick they were recorded at.
    pub fn initialize(&mut self, initial_sqrt_price_x96: i128) -> Result<()> {
        let min = swap_math::tick_to_sqrt_price_x96(MIN_TICK);
        let max = swap_math::tick_to_sqrt_price_x96(MAX_TICK);
        let price =
            u128::try_from(initial_sqrt_price_x96).map_err(|_| SimulationError::InvalidPrice)?;
        if !(min..=max).contains(&price) {
            return Err(SimulationError::InvalidPrice);
        }
        self.ensure_repriceable()?;
        self.sqrt_price_x96 = initial_sqrt_price_x96;
        self.current_tick = swap_math::sqrt_price_x96_to_tick(price);
        Ok(())
    }

    /// Initialize the pool at a tick, as the contract's `initialize` does:
    /// the stored price is that tick's sqrt price.
    pub fn initialize_at_tick(&mut self, tick: i32) -> Result<()> {
        if !(MIN_TICK..=MAX_TICK).contains(&tick) {
            return Err(SimulationError::TickOutOfRange { tick });
        }
        self.ensure_repriceable()?;
        self.sqrt_price_x96 = swap_math::tick_to_sqrt_price_x96(tick) as i128;
        self.current_tick = tick;
        Ok(())
    }

    fn ensure_repriceable(&self) -> Result<()> {
        if self.ticks.is_empty() && self.liquidity == 0 {
            Ok(())
        } else {
            Err(SimulationError::InvalidInput(
                "cannot re-price a pool that holds liquidity".to_string(),
            ))
        }
    }

    /// Set the protocol's share of swap fees, in basis points (0..=10_000).
    pub fn set_protocol_fee_bps(&mut self, bps: i128) -> Result<()> {
        if !(0..=10_000).contains(&bps) {
            return Err(SimulationError::InvalidFeeBps { fee_bps: bps });
        }
        self.protocol_fee_bps = bps;
        Ok(())
    }

    /// Pause the pool.
    pub fn pause(&mut self) {
        self.paused = true;
    }

    /// Resume the pool.
    pub fn unpause(&mut self) {
        self.paused = false;
    }

    /// Identifier under which a position is stored in [`Self::positions`].
    pub fn position_key(owner: &str, lower_tick: i32, upper_tick: i32) -> String {
        format!("{owner}:{lower_tick}:{upper_tick}")
    }

    /// Add `liquidity` to `owner`'s position over `[lower_tick, upper_tick)`.
    ///
    /// Mirrors the bookkeeping of the contract's `mint_position` once it has
    /// derived the liquidity from the deposited amounts: fees accrued so far
    /// are settled into the position, both boundary ticks are updated (a
    /// newly referenced tick records fee growth outside as the global value
    /// when it is at or below the current tick, zero otherwise), and active
    /// liquidity grows if the range contains the current tick.
    ///
    /// Token amounts are not modelled here; the contract's amount-to-liquidity
    /// conversion is a separate path from the swap engine.
    pub fn add_liquidity(
        &mut self,
        owner: &str,
        lower_tick: i32,
        upper_tick: i32,
        liquidity: i128,
    ) -> Result<()> {
        if self.paused {
            return Err(SimulationError::Paused);
        }
        if lower_tick >= upper_tick {
            return Err(SimulationError::InvalidInput(format!(
                "lower tick {lower_tick} must be below upper tick {upper_tick}"
            )));
        }
        for tick in [lower_tick, upper_tick] {
            if !(MIN_TICK..=MAX_TICK).contains(&tick) {
                return Err(SimulationError::TickOutOfRange { tick });
            }
            if tick % self.tick_spacing != 0 {
                return Err(SimulationError::InvalidInput(format!(
                    "tick {tick} is not a multiple of tick spacing {}",
                    self.tick_spacing
                )));
            }
        }
        if liquidity <= 0 {
            return Err(SimulationError::ZeroAmount);
        }

        let (inside_a, inside_b) = self.fee_growth_inside(lower_tick, upper_tick);
        let key = Self::position_key(owner, lower_tick, upper_tick);
        let position = self.positions.entry(key).or_insert_with(|| Position {
            owner: owner.to_string(),
            lower_tick,
            upper_tick,
            liquidity: 0,
            fee_growth_inside_a_snapshot: inside_a,
            fee_growth_inside_b_snapshot: inside_b,
            tokens_owed_a: 0,
            tokens_owed_b: 0,
        });
        let (owed_a, owed_b) = pending_fees(position, inside_a, inside_b);
        position.tokens_owed_a += owed_a;
        position.tokens_owed_b += owed_b;
        position.fee_growth_inside_a_snapshot = inside_a;
        position.fee_growth_inside_b_snapshot = inside_b;
        position.liquidity += liquidity;

        self.update_tick(lower_tick, liquidity, false);
        self.update_tick(upper_tick, liquidity, true);

        if self.current_tick >= lower_tick && self.current_tick < upper_tick {
            self.liquidity += liquidity;
        }
        Ok(())
    }

    /// Mirrors the contract's `update_tick` for a liquidity increase.
    fn update_tick(&mut self, tick: i32, liquidity_delta: i128, is_upper: bool) {
        let current_tick = self.current_tick;
        let (fg_a, fg_b) = (self.fee_growth_global_a, self.fee_growth_global_b);
        let info = self.ticks.entry(tick).or_insert_with(|| {
            let (outside_a, outside_b) = if tick <= current_tick {
                (fg_a, fg_b)
            } else {
                (0, 0)
            };
            Tick {
                liquidity_gross: 0,
                liquidity_net: 0,
                fee_growth_outside_a: outside_a,
                fee_growth_outside_b: outside_b,
            }
        });
        info.liquidity_gross += liquidity_delta;
        if is_upper {
            info.liquidity_net -= liquidity_delta;
        } else {
            info.liquidity_net += liquidity_delta;
        }
    }

    /// Fee growth per unit of liquidity accumulated inside
    /// `[lower_tick, upper_tick)`, as `(token_a, token_b)`. Mirrors the
    /// contract's `fee_growth_inside`.
    pub fn fee_growth_inside(&self, lower_tick: i32, upper_tick: i32) -> (i128, i128) {
        let (fg_a, fg_b) = (self.fee_growth_global_a, self.fee_growth_global_b);
        let outside = |tick: i32| {
            self.ticks
                .get(&tick)
                .map(|t| (t.fee_growth_outside_a, t.fee_growth_outside_b))
                .unwrap_or((0, 0))
        };

        let (lower_a, lower_b) = outside(lower_tick);
        let (below_a, below_b) = if self.current_tick >= lower_tick {
            (lower_a, lower_b)
        } else {
            (fg_a - lower_a, fg_b - lower_b)
        };

        let (upper_a, upper_b) = outside(upper_tick);
        let (above_a, above_b) = if self.current_tick < upper_tick {
            (upper_a, upper_b)
        } else {
            (fg_a - upper_a, fg_b - upper_b)
        };

        (fg_a - below_a - above_a, fg_b - below_b - above_b)
    }

    /// Fees `owner` could collect from `[lower_tick, upper_tick)` right now:
    /// settled `tokens_owed` plus growth since the last settlement.
    pub fn position_fees(
        &self,
        owner: &str,
        lower_tick: i32,
        upper_tick: i32,
    ) -> Result<(i128, i128)> {
        let position = self
            .positions
            .get(&Self::position_key(owner, lower_tick, upper_tick))
            .ok_or_else(|| {
                SimulationError::InvalidInput(format!(
                    "no position for {owner} over [{lower_tick}, {upper_tick})"
                ))
            })?;
        let (inside_a, inside_b) = self.fee_growth_inside(lower_tick, upper_tick);
        let (owed_a, owed_b) = pending_fees(position, inside_a, inside_b);
        Ok((
            position.tokens_owed_a + owed_a,
            position.tokens_owed_b + owed_b,
        ))
    }

    /// Nearest initialized tick in the swap direction: the highest at or
    /// below `tick` when `zero_for_one`, otherwise the lowest strictly above
    /// it. Mirrors the contract's `next_initialized_tick` bitmap search.
    pub fn next_initialized_tick(&self, tick: i32, zero_for_one: bool) -> Option<i32> {
        if zero_for_one {
            self.ticks.range(..=tick).next_back().map(|(t, _)| *t)
        } else {
            self.ticks
                .range((std::ops::Bound::Excluded(tick), std::ops::Bound::Unbounded))
                .next()
                .map(|(t, _)| *t)
        }
    }
}

/// Fees earned since the position's snapshot. Mirrors `pending_fees`.
fn pending_fees(position: &Position, inside_a: i128, inside_b: i128) -> (i128, i128) {
    let delta_a = inside_a - position.fee_growth_inside_a_snapshot;
    let delta_b = inside_b - position.fee_growth_inside_b_snapshot;
    let owed = |delta: i128| {
        if delta > 0 {
            position.liquidity * delta / FEE_GROWTH_SCALE
        } else {
            0
        }
    };
    (owed(delta_a), owed(delta_b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> ClPoolState {
        ClPoolState::new("A", "B", 30, 10).unwrap()
    }

    #[test]
    fn new_pool_tick_matches_its_price() {
        let p = pool();
        assert_eq!(p.current_tick(), 0);
        assert_eq!(
            swap_math::sqrt_price_x96_to_tick(p.sqrt_price_x96() as u128),
            p.current_tick()
        );
    }

    #[test]
    fn initialize_derives_tick_from_price() {
        for tick in [
            -200_000, -46_055, -6_932, -500, -1, 0, 1, 887, 23_027, 100_000,
        ] {
            let mut p = pool();
            let price = swap_math::tick_to_sqrt_price_x96(tick);
            p.initialize(price as i128).unwrap();
            let derived = p.current_tick();
            // Floor semantics: the derived tick's price is at or below the
            // pool price and the next tick's is above it.
            assert!(swap_math::tick_to_sqrt_price_x96(derived) <= price);
            assert!(swap_math::tick_to_sqrt_price_x96(derived + 1) > price);
            assert_eq!(p.sqrt_price_x96(), price as i128);
        }
    }

    #[test]
    fn initialize_between_ticks_rounds_down() {
        for tick in [-3_000, -61, -2, 0, 5, 4_000] {
            let mut p = pool();
            let lo = swap_math::tick_to_sqrt_price_x96(tick);
            let hi = swap_math::tick_to_sqrt_price_x96(tick + 1);
            assert!(
                hi > lo + 1,
                "fixture tick {tick} must have distinct neighbours"
            );
            p.initialize(((lo + hi) / 2) as i128).unwrap();
            assert_eq!(p.current_tick(), tick);
            p = pool();
            p.initialize((lo - 1) as i128).unwrap();
            assert_eq!(p.current_tick(), tick - 1);
        }
    }

    #[test]
    fn initialize_rejects_unrepresentable_prices() {
        let mut p = pool();
        assert!(p.initialize(0).is_err());
        assert!(p.initialize(-1).is_err());
        let min = swap_math::tick_to_sqrt_price_x96(MIN_TICK) as i128;
        assert!(p.initialize(min - 1).is_err());
        let max = swap_math::tick_to_sqrt_price_x96(MAX_TICK) as i128;
        assert!(p.initialize(max + 1).is_err());
    }

    #[test]
    fn initialize_refuses_to_reprice_a_funded_pool() {
        let mut p = pool();
        p.add_liquidity("lp", -100, 100, 1_000).unwrap();
        assert!(p
            .initialize(swap_math::tick_to_sqrt_price_x96(50) as i128)
            .is_err());
        assert!(p.initialize_at_tick(50).is_err());
    }

    #[test]
    fn full_fee_is_rejected_like_the_contract() {
        assert!(ClPoolState::new("A", "B", 10_000, 1).is_err());
        assert!(ClPoolState::new("A", "B", 9_999, 1).is_ok());
    }

    #[test]
    fn add_liquidity_updates_ticks_and_active_liquidity() {
        let mut p = pool();
        p.add_liquidity("lp", -100, 100, 1_000).unwrap();
        p.add_liquidity("lp", 100, 200, 500).unwrap();
        assert_eq!(p.liquidity, 1_000);
        assert_eq!(p.ticks[&-100].liquidity_net, 1_000);
        assert_eq!(p.ticks[&100].liquidity_net, -500);
        assert_eq!(p.ticks[&100].liquidity_gross, 1_500);
        assert_eq!(p.ticks[&200].liquidity_net, -500);
        assert_eq!(p.next_initialized_tick(0, true), Some(-100));
        assert_eq!(p.next_initialized_tick(0, false), Some(100));
        assert_eq!(p.next_initialized_tick(100, false), Some(200));
        assert_eq!(p.next_initialized_tick(-101, true), None);
    }

    #[test]
    fn add_liquidity_validates_range() {
        let mut p = pool();
        assert!(p.add_liquidity("lp", 100, -100, 1).is_err());
        assert!(p.add_liquidity("lp", -105, 100, 1).is_err());
        assert!(p.add_liquidity("lp", -100, 100, 0).is_err());
        p.pause();
        assert!(matches!(
            p.add_liquidity("lp", -100, 100, 1),
            Err(SimulationError::Paused)
        ));
    }
}
