//! Tick math for concentrated liquidity pools.
//!
//! Implements the mathematical operations for converting between ticks and prices,
//! and calculating token amounts for liquidity changes.
//!
//! This module must produce **exact** integer results that match the on-chain contract.
//! Rounding direction is critical and tested via parity tests.

use crate::error::{Result, SimulationError};

/// Constant: 1.0001^(-887272) ≈ 0
const MIN_TICK: i32 = -887272;
/// Constant: 1.0001^887272 ≈ infinity
const MAX_TICK: i32 = 887272;

/// Constant: 2^96 (for fixed-point Q64.96 arithmetic)
const Q96: u128 = 1_u128 << 96;

/// Minimum sqrt price in Q64.96, corresponding to [`MIN_TICK`].
const MIN_SQRT_PRICE: u128 = 4_295_128_739;
/// Maximum sqrt price in Q64.96 that fits in a u128 (around tick 443636).
const MAX_SQRT_PRICE: u128 = 340_275_971_719_517_849_884_931_781_110_561_029_923;

/// Convert a tick to its corresponding sqrt_price in Q64.96 format.
///
/// Formula: sqrt_price = 1.0001^(tick/2) in Q64.96
///
/// This mirrors contracts/concentrated_liquidity/src/math.rs:tick_to_sqrt_price_x96
/// and must stay bit-for-bit identical to it — that parity is what
/// `tests/cl_parity.rs` checks.
///
/// # Errors
///
/// Returns [`SimulationError::TickOutOfRange`] outside `[MIN_TICK, MAX_TICK]`,
/// and [`SimulationError::PriceOverflow`] for the very high ticks (above about
/// 429,000) whose sqrt price fits in the contract's `u128` but not in the
/// `i128` this crate uses for prices.
pub fn tick_to_sqrt_price_x96(tick: i32) -> Result<i128> {
    if !(MIN_TICK..=MAX_TICK).contains(&tick) {
        return Err(SimulationError::TickOutOfRange { tick });
    }

    i128::try_from(tick_to_sqrt_price_x96_u128(tick)).map_err(|_| SimulationError::PriceOverflow)
}

/// Binary decomposition of `log(sqrt(1.0001))` in Q128 fixed-point, the same
/// algorithm the contract runs (Uniswap V3's `getSqrtRatioAtTick`, adapted to
/// a 128-bit word). The caller has already range-checked `tick`.
fn tick_to_sqrt_price_x96_u128(tick: i32) -> u128 {
    // Positive ticks are derived from the (exact) negative side rather than
    // inverting the Q128 ratio in u128, which would lose precision growing
    // with tick magnitude. Uses sqrt_price(t) * sqrt_price(-t) = 2^192.
    if tick > 0 {
        let inv_sqrt_price = tick_to_sqrt_price_x96_u128(-tick);
        return div_pow2(192, inv_sqrt_price).clamp(MIN_SQRT_PRICE, MAX_SQRT_PRICE);
    }

    let abs_tick = tick.unsigned_abs() as u64;

    // Each magic constant is floor(2^128 / sqrt(1.0001)^(2^k)).
    let mut ratio: u128 = if abs_tick & 0x1 != 0 {
        0xfffcb933bd6fad37aa2d162d1a594001
    } else {
        u128::MAX
    };

    macro_rules! apply_bit {
        ($bit:expr, $magic:expr) => {
            if abs_tick & (1u64 << $bit) != 0 {
                ratio = mul_shift128(ratio, $magic);
            }
        };
    }

    apply_bit!(1, 0xfff97272373d413259a46990580e213a);
    apply_bit!(2, 0xfff2e50f5f656932ef12357cf3c7fdcc);
    apply_bit!(3, 0xffe5caca7e10e4e61c3624eaa0941cd0);
    apply_bit!(4, 0xffcb9843d60f6159c9db58835c926644);
    apply_bit!(5, 0xff973b41fa98c081472e6896dfb254c0);
    apply_bit!(6, 0xff2ea16466c96a3843ec78b326b52861);
    apply_bit!(7, 0xfe5dee046a99a2a811c461f1969c3053);
    apply_bit!(8, 0xfcbe86c7900a88aedcffc83b479aa3a4);
    apply_bit!(9, 0xf987a7253ac413176f2b074cf7815e54);
    apply_bit!(10, 0xf3392b0822b70005940c7a398e4b70f3);
    apply_bit!(11, 0xe7159475a2c29b7443b29c7fa6e889d9);
    apply_bit!(12, 0xd097f3bdfd2022b8845ad8f792aa5825);
    apply_bit!(13, 0xa9f746462d870fdf8a65dc1f90e061e5);
    apply_bit!(14, 0x70d869a156d2a1b890bb3df62baf32f7);
    apply_bit!(15, 0x31be135f97d08fd981231505542fcfa6);
    apply_bit!(16, 0x9aa508b5b7a84e1c677de54f3e99bc9);
    apply_bit!(17, 0x5d6af8dedb81196699c329225ee604);
    apply_bit!(18, 0x2216e584f5fa1ea926041bedfe98);
    apply_bit!(19, 0x48a170391f7dc42444e8fa2);

    // Convert Q128 to Q96: shift right by 32 bits, rounding to nearest.
    let sqrt_price = (ratio >> 32) + u128::from((ratio & 0xFFFFFFFF) >= 0x80000000);

    sqrt_price.clamp(MIN_SQRT_PRICE, MAX_SQRT_PRICE)
}

/// Multiply two Q128 values, returning `(a * b) >> 128`.
///
/// Splits each operand into 64-bit halves so the product never needs a
/// 256-bit intermediate.
#[inline(always)]
fn mul_shift128(a: u128, b: u128) -> u128 {
    let a_hi = a >> 64;
    let a_lo = a & 0xFFFFFFFFFFFFFFFF;
    let b_hi = b >> 64;
    let b_lo = b & 0xFFFFFFFFFFFFFFFF;

    let top = a_hi * b_hi;
    let mid1 = a_hi * b_lo;
    let mid2 = a_lo * b_hi;

    let mid_sum = (mid1 >> 64).wrapping_add(mid2 >> 64);
    let mid_lo_carry = ((mid1 & 0xFFFFFFFFFFFFFFFF).wrapping_add(mid2 & 0xFFFFFFFFFFFFFFFF)) >> 64;

    top.wrapping_add(mid_sum).wrapping_add(mid_lo_carry)
}

/// Computes `floor(2^pow / d)`, saturating at `u128::MAX`.
///
/// The numerator cannot be materialised in a u128, so this does bitwise long
/// division over the single set numerator bit at position `pow`.
fn div_pow2(pow: u32, d: u128) -> u128 {
    debug_assert!(d != 0, "division by zero");
    let mut rem: u128 = 0;
    let mut quo: u128 = 0;

    for i in (0..=pow).rev() {
        // Shift in bit `i` of the numerator (set only at position `pow`).
        rem = (rem << 1) | u128::from(i == pow);
        // Appending the next quotient bit would drop the top bit: saturate.
        if quo >> 127 != 0 {
            return u128::MAX;
        }
        quo <<= 1;
        if rem >= d {
            rem -= d;
            quo |= 1;
        }
    }

    quo
}

/// Convert a sqrt_price in Q64.96 format to its floor tick: the largest tick
/// whose sqrt price is at or below `sqrt_price_x96`.
///
/// Mirrors contracts/concentrated_liquidity/src/math.rs:sqrt_price_x96_to_tick.
/// Prices outside the representable range clamp to the nearest end tick.
pub fn sqrt_price_x96_to_tick(sqrt_price_x96: i128) -> Result<i32> {
    if sqrt_price_x96 <= 0 {
        return Err(SimulationError::InvalidPrice);
    }
    let sqrt_price = (sqrt_price_x96 as u128).clamp(MIN_SQRT_PRICE, MAX_SQRT_PRICE);

    let mut lo = MIN_TICK;
    let mut hi = MAX_TICK;
    while lo < hi {
        // Bias mid upward so the loop terminates when lo + 1 == hi.
        let mid = lo + (hi - lo + 1) / 2;
        if tick_to_sqrt_price_x96_u128(mid) <= sqrt_price {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }

    Ok(lo)
}

/// Calculate the amount of token0 and token1 corresponding to a given liquidity amount.
///
/// For a position in range [lower_tick, upper_tick] at price sqrt_price:
/// - amount0 = liquidity / sqrt_price_upper
/// - amount1 = liquidity * sqrt_price_lower
///
/// This mirrors contracts/concentrated_liquidity/src/math.rs:get_amount0_delta and get_amount1_delta.
pub fn get_amount0_delta(
    sqrt_price_lower_x96: i128,
    sqrt_price_upper_x96: i128,
    liquidity: i128,
    round_up: bool,
) -> Result<i128> {
    if sqrt_price_lower_x96 <= 0
        || sqrt_price_upper_x96 <= 0
        || sqrt_price_lower_x96 >= sqrt_price_upper_x96
    {
        return Err(SimulationError::InvalidPrice);
    }

    // Formula: amount0 = liquidity * (sqrt_price_upper - sqrt_price_lower) / (sqrt_price_lower * sqrt_price_upper) / Q96
    let abs_liquidity = liquidity.unsigned_abs();
    let delta = sqrt_price_upper_x96 as u128 - sqrt_price_lower_x96 as u128;
    let scaled = abs_liquidity
        .checked_mul(delta)
        .ok_or(SimulationError::InvalidPrice)?;
    let amount = if round_up {
        (scaled / sqrt_price_lower_x96 as u128)
            .saturating_mul(Q96)
            .saturating_add(sqrt_price_upper_x96 as u128 - 1)
            / sqrt_price_upper_x96 as u128
    } else {
        (scaled / sqrt_price_lower_x96 as u128) * Q96 / sqrt_price_upper_x96 as u128
    };

    let amount = amount as i128;
    Ok(if liquidity < 0 { -amount } else { amount })
}

pub fn get_amount1_delta(
    sqrt_price_lower_x96: i128,
    sqrt_price_upper_x96: i128,
    liquidity: i128,
    round_up: bool,
) -> Result<i128> {
    if sqrt_price_lower_x96 <= 0
        || sqrt_price_upper_x96 <= 0
        || sqrt_price_lower_x96 >= sqrt_price_upper_x96
    {
        return Err(SimulationError::InvalidPrice);
    }

    // Formula: amount1 = abs(liquidity) * (sqrt_upper - sqrt_lower) / Q96.
    let numerator = liquidity
        .unsigned_abs()
        .checked_mul((sqrt_price_upper_x96 - sqrt_price_lower_x96) as u128)
        .ok_or(SimulationError::InvalidPrice)?;
    let amount = if round_up {
        numerator.div_ceil(Q96)
    } else {
        numerator / Q96
    } as i128;

    Ok(if liquidity < 0 { -amount } else { amount })
}

/// Calculate liquidity given amount0, price bounds, and a price.
pub fn get_liquidity_from_amount0(
    amount0: i128,
    sqrt_price_lower_x96: i128,
    sqrt_price_upper_x96: i128,
) -> Result<i128> {
    if amount0 <= 0 || sqrt_price_lower_x96 <= 0 || sqrt_price_upper_x96 <= 0 {
        return Err(SimulationError::InvalidAmount);
    }

    // Inverse of get_amount0_delta, evaluated in stages to avoid Q192
    // intermediate overflow.
    let delta = sqrt_price_upper_x96 as u128 - sqrt_price_lower_x96 as u128;
    if delta == 0 {
        return Err(SimulationError::InvalidPrice);
    }
    let scaled = (amount0 as u128)
        .checked_mul(sqrt_price_lower_x96 as u128)
        .ok_or(SimulationError::InvalidAmount)?
        / Q96;
    let liquidity = scaled
        .checked_mul(sqrt_price_upper_x96 as u128)
        .ok_or(SimulationError::InvalidAmount)?
        / delta;

    Ok(liquidity as i128)
}

/// Calculate liquidity given amount1, price bounds, and a price.
pub fn get_liquidity_from_amount1(
    amount1: i128,
    sqrt_price_lower_x96: i128,
    sqrt_price_upper_x96: i128,
) -> Result<i128> {
    if amount1 <= 0 || sqrt_price_lower_x96 <= 0 || sqrt_price_upper_x96 <= 0 {
        return Err(SimulationError::InvalidAmount);
    }

    // Inverse of get_amount1_delta
    let liquidity = (amount1 * Q96 as i128) / (sqrt_price_upper_x96 - sqrt_price_lower_x96);

    Ok(liquidity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_to_sqrt_price_roundtrip() {
        // The conversion should be lossless (or nearly so)
        let tick = 0;
        let price = tick_to_sqrt_price_x96(tick).unwrap();
        assert_eq!(price, Q96 as i128, "Tick 0 should map to price 1.0");
    }

    #[test]
    fn sqrt_price_to_tick_is_floor() {
        for tick in [-50_000, -887, -1, 0, 1, 887, 50_000] {
            let price = tick_to_sqrt_price_x96(tick).unwrap();
            assert_eq!(sqrt_price_x96_to_tick(price).unwrap(), tick);
            // Just above a tick's price is still that tick, not the next one.
            assert_eq!(sqrt_price_x96_to_tick(price + 1).unwrap(), tick);
            assert_eq!(sqrt_price_x96_to_tick(price - 1).unwrap(), tick - 1);
        }
    }

    #[test]
    fn get_amount_delta_consistency() {
        // Use explicit Q64.96 values so this test isolates delta arithmetic
        // from the separate tick lookup implementation.
        let sqrt_lower = (Q96 - Q96 / 10_000) as i128;
        let sqrt_upper = (Q96 + Q96 / 10_000) as i128;
        let liquidity = 1_000_000_i128;

        for round_up in [false, true] {
            let amount0 = get_amount0_delta(sqrt_lower, sqrt_upper, liquidity, round_up).unwrap();
            let amount1 = get_amount1_delta(sqrt_lower, sqrt_upper, liquidity, round_up).unwrap();

            assert!(
                amount0 > 0 && amount1 > 0,
                "delta must be positive: round_up={round_up}, amount0={amount0}, amount1={amount1}"
            );

            let liquidity0 = get_liquidity_from_amount0(amount0, sqrt_lower, sqrt_upper).unwrap();
            let liquidity1 = get_liquidity_from_amount1(amount1, sqrt_lower, sqrt_upper).unwrap();
            // Staged Q96 division can lose up to about 1% in this narrow
            // range; assert the bounded relative error with diagnostics.
            let tolerance = liquidity / 100 + 2;

            assert!(
                (liquidity0 - liquidity).abs() <= tolerance,
                "amount0 round-trip mismatch: round_up={round_up}, amount0={amount0}, liquidity={liquidity}, reconstructed={liquidity0}, tolerance={tolerance}"
            );
            assert!(
                (liquidity1 - liquidity).abs() <= tolerance,
                "amount1 round-trip mismatch: round_up={round_up}, amount1={amount1}, liquidity={liquidity}, reconstructed={liquidity1}, tolerance={tolerance}"
            );
        }
    }
}
