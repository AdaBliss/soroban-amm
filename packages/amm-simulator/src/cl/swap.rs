//! Exact-input swap engine for the concentrated-liquidity pool.
//!
//! [`ClPoolState::swap`] is a step-for-step port of the contract's
//! `ConcentratedLiquidity::swap` tick walk: the same step boundaries, the
//! same fee gross-up and rounding, the same fee-growth accrual and
//! fee-growth-outside flips on each crossing, and the same price-limit
//! handling. `tests/cl_swap_parity.rs` runs it against the real contract on
//! shared fixtures and requires identical results.
//!
//! Not modelled: the oracle-deviation guard (the contract skips it when no
//! oracle aggregator is configured), deadlines, auth, token transfers and the
//! TWAP accumulator.

use super::pool::{ClPoolState, FEE_GROWTH_SCALE};
use super::swap_math::{self, MAX_TICK, MIN_TICK};
use crate::error::{Result, SimulationError};
use serde::{Deserialize, Serialize};

/// Outcome of a [`ClPoolState::swap`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ClSwapResult {
    /// Input actually taken, including fees. Less than the requested amount
    /// when the price limit or the end of liquidity stops the swap early.
    pub amount_in: i128,
    /// Output paid to the trader.
    pub amount_out: i128,
    /// Total swap fee charged, in the input token (LP share plus protocol
    /// share).
    pub fee: i128,
    /// Protocol share of [`Self::fee`].
    pub protocol_fee: i128,
    /// Initialized ticks crossed, in crossing order.
    pub ticks_crossed: Vec<i32>,
    /// Pool sqrt price after the swap.
    pub sqrt_price_x96: i128,
    /// Pool tick after the swap.
    pub current_tick: i32,
}

fn overflow<T>(v: Option<T>) -> Result<T> {
    v.ok_or(SimulationError::Overflow)
}

impl ClPoolState {
    /// Swap `amount_in` of token A for token B (`zero_for_one`) or of token B
    /// for token A, stepping through initialized ticks.
    ///
    /// `sqrt_price_limit_x96` bounds how far the price may move; `0` means no
    /// limit, as in the contract. The swap stops early, taking only part of
    /// the input, when it reaches the limit or runs out of liquidity. Fails
    /// with [`SimulationError::SlippageExceeded`] if the output is below
    /// `min_amount_out`. On any error the pool is left unchanged.
    pub fn swap(
        &mut self,
        zero_for_one: bool,
        amount_in: i128,
        sqrt_price_limit_x96: i128,
        min_amount_out: i128,
    ) -> Result<ClSwapResult> {
        if self.paused {
            return Err(SimulationError::Paused);
        }
        if amount_in <= 0 {
            return Err(SimulationError::ZeroAmount);
        }
        let limit =
            u128::try_from(sqrt_price_limit_x96).map_err(|_| SimulationError::InvalidPrice)?;

        let fee_bps = self.fee_bps;
        let protocol_fee_bps = self.protocol_fee_bps;
        let mut amount_remaining = amount_in;
        let mut amount_out_total = 0_i128;
        let mut current_tick = self.current_tick;
        let mut active_liquidity = self.liquidity;
        let mut sqrt_price = self.sqrt_price_x96 as u128;
        let mut fg_a = self.fee_growth_global_a;
        let mut fg_b = self.fee_growth_global_b;
        let mut lp_fee_total = 0_i128;
        let mut protocol_fee_total = 0_i128;
        // (tick, new fee_growth_outside_a, new fee_growth_outside_b), applied
        // only once the swap is known to succeed.
        let mut crossings: Vec<(i32, i128, i128)> = Vec::new();

        // Splits one step's fee and books it against the side being paid in.
        let mut accrue =
            |fee: i128, active: i128, fg_a: &mut i128, fg_b: &mut i128| -> Result<()> {
                let protocol_fee = overflow(fee.checked_mul(protocol_fee_bps))? / 10_000;
                let lp_fee = fee - protocol_fee;
                protocol_fee_total += protocol_fee;
                lp_fee_total += lp_fee;
                if lp_fee > 0 {
                    let growth = overflow(lp_fee.checked_mul(FEE_GROWTH_SCALE))? / active;
                    if zero_for_one {
                        *fg_a += growth;
                    } else {
                        *fg_b += growth;
                    }
                }
                Ok(())
            };
        // Input needed for `after_fee` to reach the pool, rounded up.
        let gross_up = |after_fee: i128| -> Result<i128> {
            Ok(
                (overflow(after_fee.checked_mul(10_000))? + 10_000 - fee_bps - 1)
                    / (10_000 - fee_bps),
            )
        };

        while amount_remaining > 0 {
            let next_tick_opt = self.next_initialized_tick(current_tick, zero_for_one);
            if next_tick_opt.is_none() && active_liquidity == 0 {
                break;
            }
            let next_tick = match next_tick_opt {
                Some(t) if zero_for_one => t.max(MIN_TICK),
                Some(t) => t.min(MAX_TICK),
                None if zero_for_one => MIN_TICK,
                None => MAX_TICK,
            };
            let next_price = swap_math::tick_to_sqrt_price_x96(next_tick);

            let mut target_price = next_price;
            let mut hit_limit = false;
            if limit != 0 {
                let reached = if zero_for_one {
                    next_price <= limit
                } else {
                    next_price >= limit
                };
                if reached {
                    target_price = limit;
                    hit_limit = true;
                }
            }

            let amount_in_after_fee =
                overflow(amount_remaining.checked_mul(10_000 - fee_bps))? / 10_000;
            let (step_in_after_fee, step_out) = if active_liquidity == 0 {
                (0, 0)
            } else {
                swap_math::compute_step(active_liquidity, sqrt_price, target_price, zero_for_one)
            };

            if (amount_in_after_fee >= step_in_after_fee || active_liquidity == 0) && !hit_limit {
                // Full step: the input reaches the next initialized tick.
                let step_in = if active_liquidity > 0 && fee_bps > 0 {
                    gross_up(step_in_after_fee)?
                } else {
                    step_in_after_fee
                }
                .min(amount_remaining);

                let before = (current_tick, sqrt_price, amount_remaining);
                amount_remaining -= step_in;
                amount_out_total += step_out;
                let fee = step_in - step_in_after_fee;
                if fee > 0 && active_liquidity > 0 {
                    accrue(fee, active_liquidity, &mut fg_a, &mut fg_b)?;
                }
                sqrt_price = target_price;

                // Cross `next_tick`: flip its fee growth outside against the
                // running global value and apply its net liquidity. Prices
                // move one way within a swap, so no tick is crossed twice and
                // the committed tick state is the right one to flip.
                let net = match self.ticks.get(&next_tick) {
                    Some(info) => {
                        crossings.push((
                            next_tick,
                            fg_a - info.fee_growth_outside_a,
                            fg_b - info.fee_growth_outside_b,
                        ));
                        info.liquidity_net
                    }
                    // MIN_TICK/MAX_TICK reached with no position there.
                    None => 0,
                };
                if zero_for_one {
                    active_liquidity -= net;
                    current_tick = (next_tick - 1).max(MIN_TICK);
                } else {
                    active_liquidity += net;
                    current_tick = next_tick;
                }

                // Pinned at MIN_TICK/MAX_TICK with liquidity left, the contract
                // repeats this exact step until it exhausts its budget.
                if (current_tick, sqrt_price, amount_remaining) == before {
                    return Err(SimulationError::InvalidInput(format!(
                        "swap cannot progress past tick {current_tick}; the contract would \
                         exhaust its budget here"
                    )));
                }
            } else {
                // Partial step: the swap ends before `next_tick`.
                if active_liquidity > 0 {
                    let step_after_fee = if hit_limit {
                        step_in_after_fee
                    } else {
                        amount_in_after_fee
                    };
                    let (new_price, out) = swap_math::compute_final_price_and_output(
                        active_liquidity,
                        sqrt_price,
                        step_after_fee,
                        zero_for_one,
                    );
                    let step_in = if !hit_limit {
                        amount_remaining
                    } else if fee_bps > 0 {
                        gross_up(step_after_fee)?
                    } else {
                        step_after_fee
                    }
                    .min(amount_remaining);

                    amount_remaining -= step_in;
                    amount_out_total += out;
                    let fee = step_in - step_after_fee;
                    if fee > 0 {
                        accrue(fee, active_liquidity, &mut fg_a, &mut fg_b)?;
                    }
                    sqrt_price = if hit_limit { target_price } else { new_price };
                    // The landing price can round onto or past `next_tick`
                    // even though the step never crossed it; the contract
                    // clamps back to the uncrossed side.
                    let tick = swap_math::sqrt_price_x96_to_tick(sqrt_price);
                    current_tick = if zero_for_one {
                        tick.max(next_tick)
                    } else {
                        tick.min(next_tick)
                    };
                } else {
                    // No liquidity before the limit: move the price, trade
                    // nothing.
                    sqrt_price = target_price;
                    current_tick = swap_math::sqrt_price_x96_to_tick(sqrt_price);
                }
                break;
            }
        }

        if amount_out_total < min_amount_out {
            return Err(SimulationError::SlippageExceeded);
        }

        let ticks_crossed = crossings.iter().map(|(t, _, _)| *t).collect();
        for (tick, outside_a, outside_b) in crossings {
            if let Some(info) = self.ticks.get_mut(&tick) {
                info.fee_growth_outside_a = outside_a;
                info.fee_growth_outside_b = outside_b;
            }
        }
        self.current_tick = current_tick;
        self.liquidity = active_liquidity;
        self.sqrt_price_x96 = sqrt_price as i128;
        self.fee_growth_global_a = fg_a;
        self.fee_growth_global_b = fg_b;
        if zero_for_one {
            self.accrued_fee_a += lp_fee_total;
            self.protocol_fee_a += protocol_fee_total;
        } else {
            self.accrued_fee_b += lp_fee_total;
            self.protocol_fee_b += protocol_fee_total;
        }

        Ok(ClSwapResult {
            amount_in: amount_in - amount_remaining,
            amount_out: amount_out_total,
            fee: lp_fee_total + protocol_fee_total,
            protocol_fee: protocol_fee_total,
            ticks_crossed,
            sqrt_price_x96: self.sqrt_price_x96,
            current_tick,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cl::swap_math::tick_to_sqrt_price_x96;

    fn funded_pool() -> ClPoolState {
        let mut p = ClPoolState::new("A", "B", 30, 10).unwrap();
        p.add_liquidity("wide", -1_000, 1_000, 10_000_000_000)
            .unwrap();
        p.add_liquidity("narrow", -100, 100, 50_000_000_000)
            .unwrap();
        p
    }

    #[test]
    fn small_swap_stays_inside_the_current_range() {
        let mut p = funded_pool();
        let r = p.swap(true, 1_000_000, 0, 0).unwrap();
        assert_eq!(r.amount_in, 1_000_000);
        assert!(r.amount_out > 0 && r.amount_out < 1_000_000);
        assert!(r.ticks_crossed.is_empty());
        assert!(p.sqrt_price_x96() < tick_to_sqrt_price_x96(0) as i128);
        assert_eq!(p.current_tick(), -1);
        assert_eq!(p.liquidity, 60_000_000_000);
    }

    #[test]
    fn large_swap_crosses_ticks_and_applies_liquidity_net() {
        let mut p = funded_pool();
        let r = p.swap(true, 500_000_000, 0, 0).unwrap();
        assert_eq!(r.ticks_crossed, vec![-100]);
        assert_eq!(p.liquidity, 10_000_000_000);
        assert!(p.current_tick() < -100);

        let r = p.swap(false, 1_000_000_000, 0, 0).unwrap();
        assert_eq!(r.ticks_crossed, vec![-100, 100]);
        assert_eq!(p.liquidity, 10_000_000_000);
        assert!(p.current_tick() >= 100);
    }

    #[test]
    fn crossing_flips_fee_growth_outside() {
        let mut p = funded_pool();
        p.swap(true, 500_000_000, 0, 0).unwrap();
        let fg_a = p.fee_growth_global_a;
        assert!(fg_a > 0);
        // -100 was below the start tick, so it started at the global value
        // (0) and flips to fg_global - 0 at the moment of crossing.
        let t = &p.ticks[&-100];
        assert!(t.fee_growth_outside_a > 0 && t.fee_growth_outside_a <= fg_a);
    }

    #[test]
    fn fees_accrue_to_in_range_positions() {
        let mut p = ClPoolState::new("A", "B", 30, 10).unwrap();
        p.add_liquidity("wide", -10_000, 10_000, 1_000_000_000)
            .unwrap();
        p.add_liquidity("narrow", -5_000, 5_000, 5_000_000_000)
            .unwrap();
        let r = p.swap(true, 100_000_000, 0, 0).unwrap();
        assert!(r.ticks_crossed.is_empty());
        assert_eq!(r.fee, 300_000);
        assert_eq!(p.accrued_fee_a, 300_000);
        // 300_000 * 1e6 / 6e9 per unit of liquidity.
        assert_eq!(p.fee_growth_global_a, 50);
        assert_eq!(
            p.position_fees("wide", -10_000, 10_000).unwrap(),
            (50_000, 0)
        );
        assert_eq!(
            p.position_fees("narrow", -5_000, 5_000).unwrap(),
            (250_000, 0)
        );
        // A range the price never entered earns nothing.
        p.add_liquidity("idle", 20_000, 30_000, 1_000_000_000)
            .unwrap();
        p.swap(true, 100_000_000, 0, 0).unwrap();
        assert_eq!(p.position_fees("idle", 20_000, 30_000).unwrap(), (0, 0));
    }

    #[test]
    fn protocol_fee_is_split_out() {
        let mut p = funded_pool();
        p.set_protocol_fee_bps(2_000).unwrap();
        let r = p.swap(true, 1_000_000, 0, 0).unwrap();
        assert_eq!(r.fee, 3_000);
        assert_eq!(r.protocol_fee, 600);
        assert_eq!(p.protocol_fee_a, 600);
        assert_eq!(p.accrued_fee_a, 2_400);
    }

    #[test]
    fn price_limit_stops_the_swap_and_refunds_the_rest() {
        let mut p = funded_pool();
        let limit = tick_to_sqrt_price_x96(-50) as i128;
        let r = p.swap(true, 10_000_000_000, limit, 0).unwrap();
        assert!(r.amount_in < 10_000_000_000);
        assert_eq!(p.sqrt_price_x96(), limit);
        assert_eq!(p.current_tick(), -50);
        assert!(r.ticks_crossed.is_empty());
    }

    #[test]
    fn swap_stops_where_liquidity_ends() {
        let mut p = ClPoolState::new("A", "B", 30, 10).unwrap();
        p.add_liquidity("lp", -100, 100, 1_000_000).unwrap();
        let r = p.swap(false, i64::MAX as i128, 0, 0).unwrap();
        assert!(r.amount_in < i64::MAX as i128);
        assert_eq!(r.ticks_crossed, vec![100]);
        assert_eq!(p.liquidity, 0);
        // Nothing left to trade against in this direction.
        let r = p.swap(false, 1_000, 0, 0).unwrap();
        assert_eq!((r.amount_in, r.amount_out), (0, 0));
    }

    #[test]
    fn failed_swap_leaves_pool_unchanged() {
        let mut p = funded_pool();
        let before = p.clone();
        assert!(matches!(
            p.swap(true, 500_000_000, 0, i128::MAX),
            Err(SimulationError::SlippageExceeded)
        ));
        assert_eq!(p, before);
        assert!(p.swap(true, 0, 0, 0).is_err());
        assert!(p.swap(true, 1, -1, 0).is_err());
        p.pause();
        assert!(matches!(
            p.swap(true, 1, 0, 0),
            Err(SimulationError::Paused)
        ));
    }
}
