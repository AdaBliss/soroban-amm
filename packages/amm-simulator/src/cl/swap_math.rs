//! Price and amount math used by the concentrated-liquidity swap path.
//!
//! The on-chain pool has two tick/price mappings:
//!
//! - `contracts/concentrated_liquidity/src/math.rs::tick_to_sqrt_price_x96`,
//!   the Q128 bit-decomposition that [`super::math`] mirrors. Position math
//!   uses it.
//! - `ConcentratedLiquidity::tick_to_sqrt_price_x96` in `src/lib.rs`, built on
//!   binary exponentiation of `1.0001` in a `1e6` fixed-point scale followed by
//!   an integer square root. `initialize` stores this value as the pool price,
//!   and `swap` uses it for every tick boundary and, through its inverse, to
//!   recover the current tick after a partial step.
//!
//! The two disagree by up to a few ticks, and far more at deep negative ticks
//! where the `1e6` scale runs out of precision. A simulator that stepped
//! through ticks with the first mapping would place boundaries somewhere the
//! contract does not, so every function here mirrors the swap path exactly,
//! including its saturation and rounding. `tests/cl_swap_parity.rs` runs the
//! real contract alongside [`super::pool::ClPoolState::swap`] to hold that.

/// Lowest tick the contract accepts.
pub const MIN_TICK: i32 = -887_272;
/// Highest tick the contract accepts.
pub const MAX_TICK: i32 = 887_272;

/// `2^96`, the Q64.96 unit.
const Q96: u128 = 1 << 96;
/// Fixed-point scale of `tick_to_price` (`1.0 == PRICE_SCALE`).
const PRICE_SCALE: i128 = 1_000_000;
/// `1.0001` in `PRICE_SCALE` units.
const TICK_BASE_NUM: i128 = 1_000_100;
const TICK_BASE_DEN: i128 = PRICE_SCALE;

/// `PRICE_SCALE * 1.0001^tick`, mirroring `tick_to_price_bexp`.
///
/// Saturates rather than overflowing at extreme ticks, as the contract does.
pub fn tick_to_price(tick: i32) -> i128 {
    if tick == 0 {
        return PRICE_SCALE;
    }
    let mut price = PRICE_SCALE;
    let mut base = TICK_BASE_NUM;
    let mut exp = tick.unsigned_abs();
    while exp > 0 {
        if exp & 1 != 0 {
            price = price.saturating_mul(base) / TICK_BASE_DEN;
        }
        base = base.saturating_mul(base) / TICK_BASE_DEN;
        exp >>= 1;
    }
    if tick < 0 {
        if price <= 0 {
            1
        } else {
            (PRICE_SCALE * PRICE_SCALE) / price
        }
    } else {
        price
    }
}

/// Integer square root by Newton's method, mirroring the contract's `sqrt`.
fn isqrt(y: i128) -> i128 {
    if y > 3 {
        let mut z = y;
        let mut x = y / 2 + 1;
        while x < z {
            z = x;
            x = (y / x + x) / 2;
        }
        z
    } else if y != 0 {
        1
    } else {
        0
    }
}

/// The pool's sqrt price at `tick`, in Q64.96.
///
/// Mirrors `ConcentratedLiquidity::tick_to_sqrt_price_x96`: out-of-range ticks
/// are clamped, and prices above roughly tick 167,000 saturate to a single
/// value because the contract's `saturating_mul` does.
pub fn tick_to_sqrt_price_x96(tick: i32) -> u128 {
    let tick = tick.clamp(MIN_TICK, MAX_TICK);
    let price_scaled = tick_to_price(tick).saturating_mul(1_000_000).max(1);
    (isqrt(price_scaled) as u128).saturating_mul(Q96) / 1_000_000
}

/// Largest tick `t` with `tick_to_sqrt_price_x96(t) <= sqrt_price_x96`.
///
/// Mirrors `ConcentratedLiquidity::sqrt_price_x96_to_tick`, including mapping
/// a zero price to [`MIN_TICK`]. Because the forward mapping is coarse at deep
/// negative ticks, several adjacent ticks can share a price; the largest one
/// is returned.
pub fn sqrt_price_x96_to_tick(sqrt_price_x96: u128) -> i32 {
    if sqrt_price_x96 == 0 {
        return MIN_TICK;
    }
    let mut low = MIN_TICK;
    let mut high = MAX_TICK;
    while low < high {
        let mid = low + (high - low + 1) / 2;
        if tick_to_sqrt_price_x96(mid) <= sqrt_price_x96 {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

// ---------------------------------------------------------------------------
// 256-bit intermediate helpers (contract math.rs: mul_wide / div_wide / mul_div)
// ---------------------------------------------------------------------------

const MASK64: u128 = 0xFFFF_FFFF_FFFF_FFFF;

/// Full-width `a * b` as `(hi, lo)` 128-bit halves.
fn mul_wide(a: u128, b: u128) -> (u128, u128) {
    let (a_hi, a_lo) = (a >> 64, a & MASK64);
    let (b_hi, b_lo) = (b >> 64, b & MASK64);

    let ll = a_lo * b_lo;
    let lh = a_lo * b_hi;
    let hl = a_hi * b_lo;
    let hh = a_hi * b_hi;

    let mid = (ll >> 64) + (lh & MASK64) + (hl & MASK64);

    let lo = (ll & MASK64) | (mid << 64);
    let hi = hh + (lh >> 64) + (hl >> 64) + (mid >> 64);
    (hi, lo)
}

/// `floor((hi:lo) / d)`; `None` when `d == 0` or the quotient overflows.
fn div_wide(hi: u128, lo: u128, d: u128) -> Option<u128> {
    if d == 0 || hi >= d {
        return None;
    }

    let mut rem = hi;
    let mut quo: u128 = 0;

    for i in (0..128).rev() {
        let carry = rem >> 127;
        rem = (rem << 1) | ((lo >> i) & 1);
        quo <<= 1;
        if carry == 1 || rem >= d {
            rem = rem.wrapping_sub(d);
            quo |= 1;
        }
    }

    Some(quo)
}

/// `floor(a * b / d)` over a 256-bit intermediate.
pub fn mul_div(a: u128, b: u128, d: u128) -> Option<u128> {
    let (hi, lo) = mul_wide(a, b);
    if hi == 0 {
        return lo.checked_div(d);
    }
    div_wide(hi, lo, d)
}

/// `ceil(a * b / d)` over a 256-bit intermediate.
pub fn mul_div_ceil(a: u128, b: u128, d: u128) -> Option<u128> {
    let q = mul_div(a, b, d)?;
    if mul_wide(q, d) == mul_wide(a, b) {
        Some(q)
    } else {
        q.checked_add(1)
    }
}

/// Exact `amount0` between two sqrt prices; `round_up` rounds toward the pool.
pub fn amount0_delta_exact(
    mut sqrt_a: u128,
    mut sqrt_b: u128,
    liquidity: u128,
    round_up: bool,
) -> Option<u128> {
    if sqrt_a > sqrt_b {
        core::mem::swap(&mut sqrt_a, &mut sqrt_b);
    }
    if sqrt_a == 0 || sqrt_a == sqrt_b || liquidity == 0 {
        return Some(0);
    }
    let delta = sqrt_b - sqrt_a;
    if round_up {
        let t = mul_div_ceil(liquidity, Q96, sqrt_a)?;
        mul_div_ceil(t, delta, sqrt_b)
    } else {
        let t = mul_div(liquidity, Q96, sqrt_a)?;
        mul_div(t, delta, sqrt_b)
    }
}

/// Exact `amount1` between two sqrt prices; `round_up` rounds toward the pool.
pub fn amount1_delta_exact(
    mut sqrt_a: u128,
    mut sqrt_b: u128,
    liquidity: u128,
    round_up: bool,
) -> Option<u128> {
    if sqrt_a > sqrt_b {
        core::mem::swap(&mut sqrt_a, &mut sqrt_b);
    }
    if sqrt_a == sqrt_b || liquidity == 0 {
        return Some(0);
    }
    let delta = sqrt_b - sqrt_a;
    if round_up {
        mul_div_ceil(liquidity, delta, Q96)
    } else {
        mul_div(liquidity, delta, Q96)
    }
}

/// Next sqrt price after adding `amount0` (price falls, rounded up).
pub fn next_sqrt_price_from_amount0_in(
    sqrt_p: u128,
    liquidity: u128,
    amount0: u128,
) -> Option<u128> {
    if amount0 == 0 {
        return Some(sqrt_p);
    }
    if liquidity == 0 {
        return None;
    }
    let term = mul_div(amount0, sqrt_p, Q96)?;
    let denom = liquidity.checked_add(term)?;
    mul_div_ceil(liquidity, sqrt_p, denom)
}

/// Next sqrt price after adding `amount1` (price rises, rounded down).
pub fn next_sqrt_price_from_amount1_in(
    sqrt_p: u128,
    liquidity: u128,
    amount1: u128,
) -> Option<u128> {
    if amount1 == 0 {
        return Some(sqrt_p);
    }
    if liquidity == 0 {
        return None;
    }
    let rise = mul_div(amount1, Q96, liquidity)?;
    sqrt_p.checked_add(rise)
}

/// Clamp an optional wide result into `i128`; overflow becomes 0, as in the
/// contract's `u128_to_i128_saturating`.
fn to_i128_or_zero(v: Option<u128>) -> i128 {
    match v {
        Some(x) if x <= i128::MAX as u128 => x as i128,
        _ => 0,
    }
}

/// `(amount_in_after_fee, amount_out)` needed to move the price from
/// `sqrt_current` all the way to `sqrt_target`. Mirrors `compute_step`.
pub fn compute_step(
    liquidity: i128,
    sqrt_current: u128,
    sqrt_target: u128,
    zero_for_one: bool,
) -> (i128, i128) {
    if liquidity <= 0 || sqrt_current == sqrt_target {
        return (0, 0);
    }
    let liq = liquidity.unsigned_abs();

    let (lo, hi) = if zero_for_one {
        if sqrt_target >= sqrt_current {
            return (0, 0);
        }
        (sqrt_target, sqrt_current)
    } else {
        if sqrt_target <= sqrt_current {
            return (0, 0);
        }
        (sqrt_current, sqrt_target)
    };

    let (amount_in, amount_out) = if zero_for_one {
        (
            amount0_delta_exact(lo, hi, liq, true),
            amount1_delta_exact(lo, hi, liq, false),
        )
    } else {
        (
            amount1_delta_exact(lo, hi, liq, true),
            amount0_delta_exact(lo, hi, liq, false),
        )
    };

    (to_i128_or_zero(amount_in), to_i128_or_zero(amount_out))
}

/// Landing price and output for a step that consumes `amount_in_after_fee`
/// without reaching the next tick. Mirrors `compute_final_price_and_output`.
pub fn compute_final_price_and_output(
    liquidity: i128,
    sqrt_current: u128,
    amount_in_after_fee: i128,
    zero_for_one: bool,
) -> (u128, i128) {
    if liquidity <= 0 || amount_in_after_fee <= 0 {
        return (sqrt_current, 0);
    }
    let liq = liquidity.unsigned_abs();
    let amt = amount_in_after_fee.unsigned_abs();

    if zero_for_one {
        let next = match next_sqrt_price_from_amount0_in(sqrt_current, liq, amt) {
            Some(p) if p <= sqrt_current => p,
            _ => return (sqrt_current, 0),
        };
        let out = to_i128_or_zero(amount1_delta_exact(next, sqrt_current, liq, false));
        (next, out)
    } else {
        let next = match next_sqrt_price_from_amount1_in(sqrt_current, liq, amt) {
            Some(p) if p >= sqrt_current => p,
            _ => return (sqrt_current, 0),
        };
        let out = to_i128_or_zero(amount0_delta_exact(sqrt_current, next, liq, false));
        (next, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_zero_is_price_one() {
        assert_eq!(tick_to_sqrt_price_x96(0), Q96);
        assert_eq!(sqrt_price_x96_to_tick(Q96), 0);
    }

    #[test]
    fn inverse_returns_floor_tick() {
        for tick in [-20_000, -1_000, -60, -1, 0, 1, 60, 1_000, 20_000] {
            let p = tick_to_sqrt_price_x96(tick);
            let back = sqrt_price_x96_to_tick(p);
            // `back` is the largest tick sharing this price, so it can only be
            // at or above `tick`, and it must map to exactly the same price.
            assert!(back >= tick, "tick {tick} -> {back}");
            assert_eq!(tick_to_sqrt_price_x96(back), p);
            assert!(tick_to_sqrt_price_x96(back + 1) > p);
            // One unit below the price belongs to a lower tick.
            assert!(sqrt_price_x96_to_tick(p - 1) < tick);
        }
    }

    #[test]
    fn zero_price_maps_to_min_tick() {
        assert_eq!(sqrt_price_x96_to_tick(0), MIN_TICK);
    }

    #[test]
    fn mul_div_ceil_rounds_only_inexact_results() {
        assert_eq!(mul_div_ceil(6, 4, 3), Some(8));
        assert_eq!(mul_div_ceil(7, 4, 3), Some(10));
        assert_eq!(mul_div(u128::MAX, 2, 4), Some(u128::MAX / 2));
    }
}
