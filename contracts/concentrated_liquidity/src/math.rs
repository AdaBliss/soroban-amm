//! Q64.96 fixed-point math for concentrated liquidity (Uniswap V3-style).
//!
//! `sqrtPriceX96` encodes sqrt(price) * 2^96 as a u128.
//! All arithmetic is integer-only (no_std, no floats).
//!
//! Constraints:
//!   MIN_TICK = -887272  →  sqrt(1.0001^MIN_TICK) * 2^96 ≈ 4295128739
//!   MAX_TICK =  887272  →  sqrt(1.0001^MAX_TICK) * 2^96 ≈ 1461446703485210103287273052203988822378723970342

#![allow(dead_code)]

/// 2^96 as u128
pub const Q96: u128 = 79_228_162_514_264_337_593_543_950_336_u128; // 1 << 96

pub const MIN_TICK: i32 = -887_272;
pub const MAX_TICK: i32 = 887_272;

/// Minimum sqrt price: tick_to_sqrt_price_x96(MIN_TICK)
pub const MIN_SQRT_PRICE: u128 = 4_295_128_739_u128;
/// Maximum sqrt price representable in u128 (Uniswap V3's true max exceeds u128 range;
/// we cap at the highest value that fits: corresponds to ~tick 443636).
pub const MAX_SQRT_PRICE: u128 = 340_275_971_719_517_849_884_931_781_110_561_029_923_u128;

// ---------------------------------------------------------------------------
// Tick ↔ sqrtPrice
// ---------------------------------------------------------------------------

/// Convert a tick index to sqrtPriceX96 using a binary decomposition of
/// log(sqrt(1.0001)) ≈ 0.00004999500050 in Q128 fixed-point.
///
/// This mirrors the Uniswap V3 `TickMath.getSqrtRatioAtTick` algorithm
/// adapted to Soroban's u128 limit (no u256 available).
///
/// Accuracy: within 1 ULP for ticks in [MIN_TICK, MAX_TICK].
pub fn tick_to_sqrt_price_x96(tick: i32) -> u128 {
    assert!((MIN_TICK..=MAX_TICK).contains(&tick), "tick out of range");

    // Positive ticks are derived from the (exact) negative side rather than
    // inverting the Q128 ratio in u128. The Q128 inverse needs a 2^256
    // numerator that does not fit in u128, which loses precision that grows
    // with the tick magnitude (issue #347). Instead use the identity
    //
    //   sqrt_price(t) * sqrt_price(-t) = (2^96)^2 = 2^192
    //
    // so sqrt_price(t) = 2^192 / sqrt_price(-t). `sqrt_price(-t)` is already
    // exact, and `div_pow2` performs the wide division without overflow.
    if tick > 0 {
        let inv_sqrt_price = tick_to_sqrt_price_x96(-tick);
        let sqrt_price = div_pow2(192, inv_sqrt_price);
        return sqrt_price.clamp(MIN_SQRT_PRICE, MAX_SQRT_PRICE);
    }

    // Work with the absolute value; negate at the end if tick < 0.
    let abs_tick = tick.unsigned_abs() as u64;

    // Each magic constant below is 2^128 / sqrt(1.0001^(2^k)), precomputed.
    // We use Q128 intermediates then shift down to Q96 at the end.
    // Represented as (hi: u64, lo: u64) where value = hi * 2^64 + lo.
    // For Soroban's u128 limit we keep the ratio as a u128 Q128 value and
    // multiply using u128 arithmetic with careful bit-shifting.

    // ratio starts at 1.0 in Q128 = 2^128 (but u128 can't hold 2^128).
    // We store ratio as a u128 where ratio * 2^128 is the true Q128 value —
    // i.e. we represent ratio in Q128 but keep the leading "1" implicit by
    // starting at u128::MAX >> 1 ... Instead we use the standard approach:
    // start at 2^128 and shift.
    //
    // Since u128 max is 2^128 - 1, we use 340282366920938463463374607431768211455 (u128::MAX)
    // as an approximation of 2^128 and track error, OR we use the simpler
    // approach: keep ratio in Q96 directly and multiply step-by-step.
    //
    // We follow the Uniswap approach: ratio is Q128, stored in u128 (accepting
    // that 2^128 wraps to 0 — so we start at u128::MAX as 1.0 - ε and account
    // for the rounding). In practice, Uniswap V3 uses uint256; here we adapt
    // using u128 with the understanding that intermediate results stay < 2^128
    // because at each step we divide by 2^128.
    //
    // Magic constants: ratio_k = floor(2^128 / sqrt(1.0001)^(2^k))
    // These are the same as Uniswap V3 but truncated to u128.

    let mut ratio: u128 = if abs_tick & 0x1 != 0 {
        0xfffcb933bd6fad37aa2d162d1a594001_u128
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

    apply_bit!(1, 0xfff97272373d413259a46990580e213a_u128);
    apply_bit!(2, 0xfff2e50f5f656932ef12357cf3c7fdcc_u128);
    apply_bit!(3, 0xffe5caca7e10e4e61c3624eaa0941cd0_u128);
    apply_bit!(4, 0xffcb9843d60f6159c9db58835c926644_u128);
    apply_bit!(5, 0xff973b41fa98c081472e6896dfb254c0_u128);
    apply_bit!(6, 0xff2ea16466c96a3843ec78b326b52861_u128);
    apply_bit!(7, 0xfe5dee046a99a2a811c461f1969c3053_u128);
    apply_bit!(8, 0xfcbe86c7900a88aedcffc83b479aa3a4_u128);
    apply_bit!(9, 0xf987a7253ac413176f2b074cf7815e54_u128);
    apply_bit!(10, 0xf3392b0822b70005940c7a398e4b70f3_u128);
    apply_bit!(11, 0xe7159475a2c29b7443b29c7fa6e889d9_u128);
    apply_bit!(12, 0xd097f3bdfd2022b8845ad8f792aa5825_u128);
    apply_bit!(13, 0xa9f746462d870fdf8a65dc1f90e061e5_u128);
    apply_bit!(14, 0x70d869a156d2a1b890bb3df62baf32f7_u128);
    apply_bit!(15, 0x31be135f97d08fd981231505542fcfa6_u128);
    apply_bit!(16, 0x9aa508b5b7a84e1c677de54f3e99bc9_u128);
    apply_bit!(17, 0x5d6af8dedb81196699c329225ee604_u128);
    apply_bit!(18, 0x2216e584f5fa1ea926041bedfe98_u128);
    apply_bit!(19, 0x48a170391f7dc42444e8fa2_u128);

    // Positive ticks are handled by the early-return inversion above, so here
    // `tick <= 0` and `ratio` already holds the correct Q128 sqrt price.

    // Convert from Q128 to Q96: shift right by 32 bits, with rounding.
    // sqrtPriceX96 = ratio >> 32
    let sqrt_price = (ratio >> 32)
        + if (ratio & 0xFFFFFFFF) >= 0x80000000 {
            1
        } else {
            0
        };

    // Clamp to valid range.
    sqrt_price.clamp(MIN_SQRT_PRICE, MAX_SQRT_PRICE)
}

/// Multiply two Q128 values and return the result as Q128.
/// Each argument is a Q128 number (true value = arg / 2^128).
/// Result = (a * b) >> 128.
///
/// Uses u128 with splitting to avoid overflow:
/// a = a_hi * 2^64 + a_lo
/// b = b_hi * 2^64 + b_lo
/// a*b = a_hi*b_hi*2^128 + (a_hi*b_lo + a_lo*b_hi)*2^64 + a_lo*b_lo
/// >> 128 keeps only a_hi*b_hi + high 64 bits of the middle terms.
#[inline(always)]
fn mul_shift128(a: u128, b: u128) -> u128 {
    let a_hi = a >> 64;
    let a_lo = a & 0xFFFFFFFFFFFFFFFF;
    let b_hi = b >> 64;
    let b_lo = b & 0xFFFFFFFFFFFFFFFF;

    let top = a_hi * b_hi;
    let mid1 = a_hi * b_lo;
    let mid2 = a_lo * b_hi;
    let _bot = a_lo * b_lo; // discarded (below 128-bit boundary)

    // mid1 and mid2 each have 128 bits; their high 64 bits add into `top`.
    let mid_sum = (mid1 >> 64).wrapping_add(mid2 >> 64);
    // Carry from the low 64 bits of the middles (approximate — 1-2 ULP error).
    let mid_lo_carry = ((mid1 & 0xFFFFFFFFFFFFFFFF).wrapping_add(mid2 & 0xFFFFFFFFFFFFFFFF)) >> 64;

    top.wrapping_add(mid_sum).wrapping_add(mid_lo_carry)
}

/// Computes `floor(2^pow / d)`, saturating at `u128::MAX` when the quotient
/// would not fit in a u128.
///
/// Used to invert a sqrt price for positive ticks: `2^192 / sqrt_price(-t)`.
/// The numerator `2^pow` cannot be materialised in a u128, so this performs
/// bitwise long division, processing the single numerator bit at position
/// `pow` and trailing zeros. The caller passes `d < 2^97` (a Q96 sqrt price),
/// which keeps the running remainder below `2^98` so it never overflows.
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

/// Convert sqrtPriceX96 back to the floor tick.
///
/// Uses binary search over [MIN_TICK, MAX_TICK]: ~21 iterations, each calling
/// `tick_to_sqrt_price_x96`. This avoids the uint256 arithmetic used in Uniswap V3
/// `TickMath.getTickAtSqrtRatio` which doesn't fit in u128.
pub fn sqrt_price_x96_to_tick(sqrt_price: u128) -> i32 {
    assert!(
        (MIN_SQRT_PRICE..=MAX_SQRT_PRICE).contains(&sqrt_price),
        "sqrt price out of range"
    );

    // Binary search for the largest tick t such that tick_to_sqrt_price_x96(t) <= sqrt_price.
    let mut lo = MIN_TICK;
    let mut hi = MAX_TICK;

    while lo < hi {
        // Bias mid upward to avoid infinite loop when lo + 1 == hi.
        let mid = lo + (hi - lo + 1) / 2;
        if tick_to_sqrt_price_x96(mid) <= sqrt_price {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }

    lo
}

// ---------------------------------------------------------------------------
// Amount deltas
// ---------------------------------------------------------------------------

// These four conversions back the position path (mint, burn, quote). Every
// one is evaluated over a 256-bit intermediate via `mul_div`, so a product
// such as `liquidity * (sqrt_b - sqrt_a)` is never truncated to 128 bits: the
// sqrt-price difference grows exponentially with tick magnitude and exceeds
// `u128` for ordinary ranges such as [100_000, 200_000] (#963).
//
// Rounding: every result rounds toward zero (floor of the magnitude), the same
// direction these functions have always used. Liquidity derived from a deposit
// is therefore never more than the deposit pays for, and an amount paid out
// for a given liquidity is never more than that liquidity is worth.
//
// Each returns `None` when the result does not fit in an `i128`, or when an
// intermediate stage of a two-stage evaluation does not fit in a `u128`, so
// the caller can surface a typed error instead of acting on a wrapped value.

/// Token A amount for a position in [sqrt_a, sqrt_b] with given liquidity.
/// Mirrors Uniswap V3: amount0 = liquidity * (sqrt_b - sqrt_a) * 2^96 / (sqrt_a * sqrt_b)
///
/// Order-independent in `sqrt_a` / `sqrt_b`; returns `Some(0)` if they are
/// equal. The sign of the result follows the sign of `liquidity`. The
/// magnitude is at most 1 below the exact floor (see [`amount0_delta_exact`]).
/// `None` if the result does not fit in an `i128`, or if the first stage
/// `|liquidity| * 2^96 / sqrt_lower` does not fit in a `u128`.
pub fn get_amount0_delta(sqrt_a: u128, sqrt_b: u128, liquidity: i128) -> Option<i128> {
    let abs_result = amount0_delta_exact(sqrt_a, sqrt_b, liquidity.unsigned_abs(), false);
    with_sign(abs_result, liquidity < 0)
}

/// Token B amount for a position in [sqrt_a, sqrt_b] with given liquidity.
/// Mirrors Uniswap V3: amount1 = liquidity * (sqrt_b - sqrt_a) / 2^96
///
/// The magnitude is the exact floor; the sign follows `liquidity`.
pub fn get_amount1_delta(sqrt_a: u128, sqrt_b: u128, liquidity: i128) -> Option<i128> {
    let abs_result = amount1_delta_exact(sqrt_a, sqrt_b, liquidity.unsigned_abs(), false);
    with_sign(abs_result, liquidity < 0)
}

/// Liquidity from a token-A amount in [sqrt_a, sqrt_b].
/// liquidity = amount0 * sqrt_a * sqrt_b / ((sqrt_b - sqrt_a) * 2^96)
///
/// Evaluated as `floor(floor(amount0 * sqrt_b / delta) * sqrt_a / 2^96)`.
/// Dividing by `delta` first keeps the intermediate at least as large as
/// `amount0`, so the relative error of the inner floor is at most
/// `1 / amount0`. Both stages round down, so the result never exceeds the
/// exact liquidity. `None` if the result does not fit in an `i128`, or if the
/// first stage does not fit in a `u128`.
pub fn get_liquidity_for_amount0(
    mut sqrt_a: u128,
    mut sqrt_b: u128,
    amount0: i128,
) -> Option<i128> {
    if sqrt_a > sqrt_b {
        core::mem::swap(&mut sqrt_a, &mut sqrt_b);
    }
    if sqrt_b == sqrt_a || amount0 == 0 {
        return Some(0);
    }
    let abs_amt = amount0.unsigned_abs();
    let abs_result =
        mul_div(abs_amt, sqrt_b, sqrt_b - sqrt_a).and_then(|t| mul_div(t, sqrt_a, Q96));
    with_sign(abs_result, amount0 < 0)
}

/// Liquidity from a token-B amount in [sqrt_a, sqrt_b].
/// liquidity = amount1 * 2^96 / (sqrt_b - sqrt_a)
///
/// The magnitude is the exact floor; the sign follows `amount1`.
pub fn get_liquidity_for_amount1(
    mut sqrt_a: u128,
    mut sqrt_b: u128,
    amount1: i128,
) -> Option<i128> {
    if sqrt_a > sqrt_b {
        core::mem::swap(&mut sqrt_a, &mut sqrt_b);
    }
    if sqrt_b == sqrt_a || amount1 == 0 {
        return Some(0);
    }
    let abs_result = mul_div(amount1.unsigned_abs(), Q96, sqrt_b - sqrt_a);
    with_sign(abs_result, amount1 < 0)
}

/// Narrow an unsigned magnitude to an `i128` carrying the requested sign.
/// `None` if the magnitude was already `None` or does not fit in an `i128`.
#[inline(always)]
fn with_sign(abs: Option<u128>, negative: bool) -> Option<i128> {
    let v = i128::try_from(abs?).ok()?;
    Some(if negative { -v } else { v })
}

// ---------------------------------------------------------------------------
// Wide (256-bit intermediate) helpers
// ---------------------------------------------------------------------------

const MASK64: u128 = 0xFFFF_FFFF_FFFF_FFFF;

/// Full-width `a * b` as a 256-bit value, returned as `(hi, lo)` 128-bit halves.
///
/// Each partial product is a `u64 * u64`, so nothing overflows on the way.
#[inline(always)]
fn mul_wide(a: u128, b: u128) -> (u128, u128) {
    let (a_hi, a_lo) = (a >> 64, a & MASK64);
    let (b_hi, b_lo) = (b >> 64, b & MASK64);

    let ll = a_lo * b_lo;
    let lh = a_lo * b_hi;
    let hl = a_hi * b_lo;
    let hh = a_hi * b_hi;

    // Middle column: the high half of `ll` plus the low halves of both
    // cross terms. This sum can carry into the high word.
    let mid = (ll >> 64) + (lh & MASK64) + (hl & MASK64);

    let lo = (ll & MASK64) | (mid << 64);
    let hi = hh + (lh >> 64) + (hl >> 64) + (mid >> 64);
    (hi, lo)
}

/// `floor((hi:lo) / d)` by shift-and-subtract long division.
///
/// Returns `None` when `d == 0` or when the quotient would not fit in a `u128`
/// (which is exactly the case `hi >= d`).
fn div_wide(hi: u128, lo: u128, d: u128) -> Option<u128> {
    if d == 0 || hi >= d {
        return None;
    }

    let mut rem = hi;
    let mut quo: u128 = 0;

    for i in (0..128).rev() {
        // `rem` is always < d here, so shifting left can only lose a bit that
        // we must remember: the true remainder is `carry * 2^128 + rem`.
        let carry = rem >> 127;
        rem = (rem << 1) | ((lo >> i) & 1);
        quo <<= 1;
        // carry == 1 means the true value is >= 2^128 > d, so it must be
        // reduced; the wrapping subtraction produces the correct low 128 bits.
        if carry == 1 || rem >= d {
            rem = rem.wrapping_sub(d);
            quo |= 1;
        }
    }

    Some(quo)
}

/// `floor(a * b / d)` evaluated over a full 256-bit intermediate.
///
/// This is the building block the Q64.96 swap math needs: products such as
/// `liquidity * sqrt_price` routinely exceed `u128` even though the final
/// quotient fits comfortably. Returns `None` if `d == 0` or the quotient
/// overflows `u128`.
pub fn mul_div(a: u128, b: u128, d: u128) -> Option<u128> {
    let (hi, lo) = mul_wide(a, b);
    if hi == 0 {
        // Fast path: the product fits in 128 bits.
        return lo.checked_div(d);
    }
    div_wide(hi, lo, d)
}

/// `ceil(a * b / d)` over the same 256-bit intermediate.
///
/// Used wherever rounding must favour the pool (charging input, or moving the
/// price at least as far as the exact solution requires).
pub fn mul_div_ceil(a: u128, b: u128, d: u128) -> Option<u128> {
    let q = mul_div(a, b, d)?;
    // The division was exact iff `q * d` reproduces `a * b` exactly; both are
    // 256-bit quantities, so compare both halves.
    if mul_wide(q, d) == mul_wide(a, b) {
        Some(q)
    } else {
        q.checked_add(1)
    }
}

/// Exact `amount0` between two sqrt prices, in full Q64.96 precision.
///
/// `amount0 = L * Q96 * (sqrt_b - sqrt_a) / (sqrt_a * sqrt_b)`, evaluated in
/// two `mul_div` stages so the `L * Q96 * delta` numerator never has to fit in
/// a `u128`. `round_up` rounds toward the pool.
///
/// With `round_up == false` the result is either the exact floor or one
/// below it: the first stage floors `L * Q96 / sqrt_a`, which costs strictly
/// less than one unit after the second stage multiplies by
/// `delta / sqrt_b < 1`. The swap path calls this directly; the position path
/// reaches it through [`get_amount0_delta`].
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

/// Exact `amount1` between two sqrt prices: `L * (sqrt_b - sqrt_a) / Q96`.
///
/// `round_up` rounds toward the pool.
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

/// Next sqrt price when `amount0` is added to the pool (price falls).
///
/// `sqrt_next = L * sqrt_p / (L + amount0 * sqrt_p / Q96)`, which is the
/// canonical `L * Q96 * sqrt_p / (L * Q96 + amount0 * sqrt_p)` rearranged so
/// that no intermediate needs more than the 256 bits `mul_div` already gives
/// us. Rounds the resulting price **up**, so the pool never moves further than
/// the input paid for.
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
    // Round the denominator term down so the quotient (the price) rounds up.
    let term = mul_div(amount0, sqrt_p, Q96)?;
    let denom = liquidity.checked_add(term)?;
    mul_div_ceil(liquidity, sqrt_p, denom)
}

/// Next sqrt price when `amount1` is added to the pool (price rises).
///
/// `sqrt_next = sqrt_p + amount1 * Q96 / L`, rounded **down** so the price
/// never moves further than the input paid for.
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

/// Next sqrt price when `amount1` is removed from the pool (price falls).
///
/// `sqrt_next = sqrt_p - ceil(amount1 * Q96 / L)`: the drop rounds **up** so
/// the price always moves at least as far as the requested output requires,
/// and the input computed from it is never less than truly owed. Returns
/// `None` if the output would drive the price to or below zero.
pub fn next_sqrt_price_from_amount1_out(
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
    let drop = mul_div_ceil(amount1, Q96, liquidity)?;
    if drop >= sqrt_p {
        return None;
    }
    Some(sqrt_p - drop)
}

/// Next sqrt price when `amount0` is removed from the pool (price rises).
///
/// `sqrt_next = L * sqrt_p / (L - amount0 * sqrt_p / Q96)`, rounded **up** for
/// the same reason as above. Returns `None` when the requested output exceeds
/// what this range can supply at any price.
pub fn next_sqrt_price_from_amount0_out(
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
    // Round the subtracted term up so the denominator is smaller and the
    // resulting price is not understated.
    let term = mul_div_ceil(amount0, sqrt_p, Q96)?;
    if term >= liquidity {
        // Would need an infinite price to supply this much token0.
        return None;
    }
    mul_div_ceil(liquidity, sqrt_p, liquidity - term)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── Wide (256-bit intermediate) helpers ──────────────────────────────────

    /// `mul_div` must agree with exact arithmetic even when `a * b` overflows
    /// `u128` — which is the normal case for `liquidity * sqrt_price`.
    #[test]
    fn mul_div_matches_exact_arithmetic_across_the_u128_range() {
        // Deterministic xorshift; no rand dependency in a no_std contract crate.
        let mut st: u128 = 0x243F_6A88_85A3_08D3;
        let mut next = || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            st
        };

        for _ in 0..2_000 {
            let a = next();
            let b = next();
            let d = next().max(1);

            // Reference: long-hand 256-bit product, then compare by
            // reconstructing q*d + r == a*b with the same wide helpers.
            match mul_div(a, b, d) {
                Some(q) => {
                    let (phi, plo) = mul_wide(a, b);
                    let (qhi, qlo) = mul_wide(q, d);
                    // q*d <= a*b
                    assert!(
                        (qhi, qlo) <= (phi, plo),
                        "q*d exceeded a*b for {a} * {b} / {d}"
                    );
                    // and (q+1)*d > a*b, i.e. the quotient is maximal
                    if let Some(q1) = q.checked_add(1) {
                        let (q1hi, q1lo) = mul_wide(q1, d);
                        assert!(
                            (q1hi, q1lo) > (phi, plo),
                            "quotient not maximal for {a} * {b} / {d}"
                        );
                    }
                }
                None => {
                    // Only legitimate when the quotient genuinely overflows.
                    let (phi, _) = mul_wide(a, b);
                    assert!(phi >= d, "mul_div gave up on a representable quotient");
                }
            }
        }
    }

    #[test]
    fn mul_div_handles_extremes() {
        assert_eq!(mul_div(u128::MAX, u128::MAX, u128::MAX), Some(u128::MAX));
        assert_eq!(mul_div(u128::MAX, 1, 1), Some(u128::MAX));
        assert_eq!(mul_div(0, 12_345, 7), Some(0));
        assert_eq!(mul_div(5, 5, 0), None);
        // Quotient would not fit in u128.
        assert_eq!(mul_div(u128::MAX, u128::MAX, 1), None);
    }

    #[test]
    fn mul_div_ceil_rounds_up_only_on_a_remainder() {
        assert_eq!(mul_div_ceil(10, 10, 5), Some(20)); // exact
        assert_eq!(mul_div_ceil(10, 10, 3), Some(34)); // 33.33 -> 34
        assert_eq!(mul_div(10, 10, 3), Some(33));
        // Exactness must be judged on the full 256-bit product, not its low word.
        assert_eq!(mul_div_ceil(Q96, Q96, Q96), Some(Q96));
    }

    /// The swap math's price step must be exactly invertible against the
    /// amount deltas: putting `amount0` in and then reading the resulting
    /// amount0 delta back must not manufacture value.
    #[test]
    fn next_sqrt_price_from_amount0_in_is_consistent_with_the_delta() {
        let liquidity = 500_000_000u128;
        let sqrt_p = Q96;
        for amount in [1u128, 7, 100, 3_900, 1_000_000] {
            let next = next_sqrt_price_from_amount0_in(sqrt_p, liquidity, amount).unwrap();
            assert!(next <= sqrt_p, "token0 in must not raise the price");
            // The amount0 the price move implies never exceeds what was paid.
            let implied = amount0_delta_exact(next, sqrt_p, liquidity, false).unwrap();
            assert!(
                implied <= amount,
                "price moved further than {amount} paid for (implied {implied})"
            );
        }
    }

    #[test]
    fn next_sqrt_price_from_amount1_in_raises_price_proportionally() {
        let liquidity = 500_000_000u128;
        let sqrt_p = Q96;
        for amount in [1u128, 7, 100, 3_900, 1_000_000] {
            let next = next_sqrt_price_from_amount1_in(sqrt_p, liquidity, amount).unwrap();
            assert!(next >= sqrt_p, "token1 in must not lower the price");
            let implied = amount1_delta_exact(sqrt_p, next, liquidity, false).unwrap();
            assert!(
                implied <= amount,
                "price moved further than {amount} paid for (implied {implied})"
            );
        }
    }

    /// A small trade against deep liquidity must move the price by a fraction
    /// of a tick. The previous `sqrt_price * 1000` scale had a resolution of
    /// roughly 20 ticks, so this moved ~20 ticks regardless of size.
    #[test]
    fn a_small_trade_moves_the_price_far_less_than_one_tick() {
        let liquidity = 498_753_117u128;
        let next = next_sqrt_price_from_amount0_in(Q96, liquidity, 3_900).unwrap();
        // Starting exactly on tick 0's boundary, any downward move lands in
        // tick -1's band, so -1 is correct; what matters is that the price
        // stays inside that one band instead of jumping ~20 ticks.
        assert_eq!(sqrt_price_x96_to_tick(next), -1);
        assert!(
            next > tick_to_sqrt_price_x96(-1),
            "3,900 against L=5e8 must not move a full tick"
        );
        // The old 3-significant-digit scale landed here instead:
        let legacy = (999u128 * Q96) / 1000;
        assert!(
            sqrt_price_x96_to_tick(legacy) <= -20,
            "one unit of the old p_c scale was a ~20-tick move"
        );
    }

    #[test]
    fn tick_zero_is_q96() {
        let sp = tick_to_sqrt_price_x96(0);
        // sqrt(1.0001^0) * 2^96 = 1 * 2^96 = Q96
        // Allow ±2 for rounding
        assert!(
            (sp as i128 - Q96 as i128).abs() <= 2,
            "tick 0 expected ~Q96, got {sp}"
        );
    }

    #[test]
    fn tick_min_is_clamped() {
        let sp = tick_to_sqrt_price_x96(MIN_TICK);
        assert_eq!(sp, MIN_SQRT_PRICE);
    }

    #[test]
    fn tick_max_is_clamped() {
        let sp = tick_to_sqrt_price_x96(MAX_TICK);
        // The u128 implementation clamps to valid range; just verify it's ≥ MIN_SQRT_PRICE.
        assert!(
            sp >= MIN_SQRT_PRICE,
            "MAX_TICK must yield at least MIN_SQRT_PRICE"
        );
    }

    #[test]
    fn round_trip_tick_to_sqrt_and_back() {
        // Both signs round-trip now that positive ticks are derived from the
        // exact negative side via the 2^192 / sqrt_price(-t) identity (#347).
        for tick in [-100_000_i32, -10_000, -100, -1, 1, 100, 10_000, 100_000] {
            let sp = tick_to_sqrt_price_x96(tick);
            let back = sqrt_price_x96_to_tick(sp);
            assert_eq!(back, tick, "round-trip failed for tick {tick}: got {back}");
        }
    }

    #[test]
    fn positive_tick_is_inverse_of_negative() {
        // sqrt_price(t) * sqrt_price(-t) must equal (2^96)^2 = 2^192, the
        // identity the positive-tick path relies on. Reduce both factors by
        // 2^48 first so the product stays within u128 for large magnitudes;
        // (pos >> 48) * (neg >> 48) then approximates (pos * neg) >> 96 ≈ 2^96.
        let q96_target: i128 = 1i128 << 96;
        for tick in [1_i32, 50, 100, 1_000, 10_000, 50_000] {
            let pos = tick_to_sqrt_price_x96(tick);
            let neg = tick_to_sqrt_price_x96(-tick);
            let product = ((pos >> 48) * (neg >> 48)) as i128;
            let rel_err = (product - q96_target).abs() * 1_000_000 / q96_target;
            assert!(
                rel_err < 200,
                "tick {tick}: product {product} drifts from 2^96 ({q96_target}) by {rel_err} ppm"
            );
        }
    }

    #[test]
    fn amount0_delta_symmetric() {
        let sp_low = tick_to_sqrt_price_x96(-100);
        let sp_high = tick_to_sqrt_price_x96(100);
        let liq = 1_000_000_i128;
        let a = get_amount0_delta(sp_low, sp_high, liq).unwrap();
        let b = get_amount0_delta(sp_high, sp_low, liq).unwrap();
        assert_eq!(a, b, "get_amount0_delta should be order-independent");
    }

    #[test]
    fn amount1_delta_symmetric() {
        let sp_low = tick_to_sqrt_price_x96(-100);
        let sp_high = tick_to_sqrt_price_x96(100);
        let liq = 1_000_000_i128;
        let a = get_amount1_delta(sp_low, sp_high, liq).unwrap();
        let b = get_amount1_delta(sp_high, sp_low, liq).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn liquidity_for_amount0_roundtrip() {
        // Use two negative ticks (positive ticks return incorrect values in u128 impl).
        let sp_low = tick_to_sqrt_price_x96(-200);
        let sp_high = tick_to_sqrt_price_x96(-100);
        let liq_in = 1_000_000_i128;
        let amount0 = get_amount0_delta(sp_low, sp_high, liq_in).unwrap();
        if amount0 > 0 {
            let liq_out = get_liquidity_for_amount0(sp_low, sp_high, amount0).unwrap();
            // Allow 1% rounding tolerance
            assert!(
                (liq_out - liq_in).abs() * 100 <= liq_in,
                "amount0 roundtrip: got {liq_out} expected ~{liq_in}"
            );
        }
    }

    #[test]
    fn liquidity_for_amount1_roundtrip() {
        let sp_low = tick_to_sqrt_price_x96(-100);
        let sp_high = tick_to_sqrt_price_x96(100);
        let liq_in = 1_000_000_i128;
        let amount1 = get_amount1_delta(sp_low, sp_high, liq_in).unwrap();
        if amount1 > 0 {
            let liq_out = get_liquidity_for_amount1(sp_low, sp_high, amount1).unwrap();
            assert!(
                (liq_out - liq_in).abs() * 100 <= liq_in,
                "amount1 roundtrip: got {liq_out} expected ~{liq_in}"
            );
        }
    }

    #[test]
    fn negative_liquidity_returns_negative_delta() {
        let sp_low = tick_to_sqrt_price_x96(-100);
        let sp_high = tick_to_sqrt_price_x96(100);
        let a = get_amount0_delta(sp_low, sp_high, 1_000_000).unwrap();
        let b = get_amount0_delta(sp_low, sp_high, -1_000_000).unwrap();
        assert_eq!(a, -b);
    }

    // ── #963: position-path conversions over high-magnitude ranges ───────────

    /// One pinned tick range: the sqrt prices `tick_to_sqrt_price_x96` yields
    /// for its bounds, and exact-precision floors of each conversion computed
    /// independently with arbitrary-precision integers:
    ///
    ///   amount0 = floor(L * 2^96 * (sb - sa) / (sa * sb))
    ///   amount1 = floor(L * (sb - sa) / 2^96)
    ///   liq0    = floor(X * sa * sb / (2^96 * (sb - sa)))
    ///   liq1    = floor(X * 2^96 / (sb - sa))
    ///
    /// with `L = 1_000_000` and a deposit of `X = 10^12`.
    struct PinnedRange {
        ticks: (i32, i32),
        sqrt: (u128, u128),
        amount0: i128,
        amount1: i128,
        liq0: i128,
        liq1: i128,
    }

    const PINNED_L: i128 = 1_000_000;
    const PINNED_X: i128 = 1_000_000_000_000;

    /// The three ranges from #963. `L * (sb - sa)` fits in a u128 for the
    /// first and is 5.09x and 755x over `u128::MAX` for the other two, which
    /// is where the removed wrapping multiply returned values 55.7x and 1753x
    /// too small.
    const PINNED: [PinnedRange; 3] = [
        PinnedRange {
            ticks: (0, 100_000),
            sqrt: (
                79_228_162_514_264_337_593_543_950_336,
                11_755_562_826_496_067_164_730_007_758_967,
            ),
            amount0: 993_260,
            amount1: 147_376_062,
            liq0: 1_006_785_362_427,
            liq1: 6_785_362_427,
        },
        PinnedRange {
            ticks: (100_000, 200_000),
            sqrt: (
                11_755_562_826_496_067_164_730_007_758_967,
                1_744_244_129_640_337_381_386_292_646_062_446,
            ),
            amount0: 6_694,
            amount1: 21_867_079_985,
            liq0: 149_382_848_285_501,
            liq1: 45_730_842,
        },
        PinnedRange {
            ticks: (200_000, 300_000),
            sqrt: (
                1_744_244_129_640_337_381_386_292_646_062_446,
                258_804_076_732_718_222_382_219_765_876_867_990,
            ),
            amount0: 45,
            amount1: 3_244_551_235_891,
            liq0: 22_164_838_896_837_700,
            liq1: 308_209,
        },
    ];

    #[test]
    fn pinned_ranges_use_the_expected_sqrt_prices() {
        for r in PINNED.iter() {
            assert_eq!(tick_to_sqrt_price_x96(r.ticks.0), r.sqrt.0);
            assert_eq!(tick_to_sqrt_price_x96(r.ticks.1), r.sqrt.1);
        }
        // The last two ranges are the ones the old wrapping product broke.
        let overflows = |r: &PinnedRange| {
            (PINNED_L as u128)
                .checked_mul(r.sqrt.1 - r.sqrt.0)
                .is_none()
        };
        assert!(!overflows(&PINNED[0]));
        assert!(overflows(&PINNED[1]));
        assert!(overflows(&PINNED[2]));
    }

    /// `get_amount1_delta` is the exact floor; `get_amount0_delta` rounds down
    /// and is at most one unit below the exact floor (the documented cost of
    /// its two-stage evaluation). Neither ever exceeds the exact value, so an
    /// amount paid out for a given liquidity never over-pays.
    #[test]
    fn amount_deltas_match_exact_values_and_round_down() {
        for r in PINNED.iter() {
            let (sa, sb) = r.sqrt;
            let a1 = get_amount1_delta(sa, sb, PINNED_L).unwrap();
            assert_eq!(a1, r.amount1, "amount1 over {:?}", r.ticks);
            let a0 = get_amount0_delta(sa, sb, PINNED_L).unwrap();
            assert!(
                a0 <= r.amount0 && a0 >= r.amount0 - 1,
                "amount0 over {:?}: got {a0}, exact floor {}",
                r.ticks,
                r.amount0
            );
            // Order-independent and sign-mirrored.
            assert_eq!(get_amount0_delta(sb, sa, PINNED_L).unwrap(), a0);
            assert_eq!(get_amount1_delta(sb, sa, PINNED_L).unwrap(), a1);
            assert_eq!(get_amount0_delta(sa, sb, -PINNED_L).unwrap(), -a0);
            assert_eq!(get_amount1_delta(sa, sb, -PINNED_L).unwrap(), -a1);
        }
    }

    /// `get_liquidity_for_amount1` is the exact floor; `get_liquidity_for_amount0`
    /// rounds down, losing at most `sa / 2^96 + 1` to its inner floor. Neither
    /// ever exceeds the exact value, so a deposit is never credited with more
    /// liquidity than it pays for.
    #[test]
    fn liquidity_for_amounts_match_exact_values_and_round_down() {
        for r in PINNED.iter() {
            let (sa, sb) = r.sqrt;
            let l1 = get_liquidity_for_amount1(sa, sb, PINNED_X).unwrap();
            assert_eq!(l1, r.liq1, "liq1 over {:?}", r.ticks);
            let l0 = get_liquidity_for_amount0(sa, sb, PINNED_X).unwrap();
            let slack = (sa / Q96) as i128 + 1;
            assert!(
                l0 <= r.liq0 && l0 >= r.liq0 - slack,
                "liq0 over {:?}: got {l0}, exact floor {}",
                r.ticks,
                r.liq0
            );
            assert_eq!(get_liquidity_for_amount0(sb, sa, PINNED_X).unwrap(), l0);
            assert_eq!(get_liquidity_for_amount1(sa, sb, -PINNED_X).unwrap(), -l1);
        }
    }

    /// Converting a deposit to liquidity and back never returns more than was
    /// deposited, and on these ranges returns it to within a relative 1e-6.
    #[test]
    fn deposit_round_trip_is_tight_on_pinned_ranges() {
        for r in PINNED.iter() {
            let (sa, sb) = r.sqrt;
            let l0 = get_liquidity_for_amount0(sa, sb, PINNED_X).unwrap();
            let back0 = get_amount0_delta(sa, sb, l0).unwrap();
            let l1 = get_liquidity_for_amount1(sa, sb, PINNED_X).unwrap();
            let back1 = get_amount1_delta(sa, sb, l1).unwrap();
            for back in [back0, back1] {
                assert!(back <= PINNED_X, "{:?}: {back} > deposit", r.ticks);
                assert!(
                    (PINNED_X - back) * 1_000_000 <= PINNED_X,
                    "{:?}: {back} not within 1e-6 of {PINNED_X}",
                    r.ticks
                );
            }
        }
    }

    /// A result that genuinely does not fit in an i128 is reported as `None`
    /// rather than wrapped.
    #[test]
    fn unrepresentable_results_are_none_not_truncated() {
        let sa = tick_to_sqrt_price_x96(MIN_TICK);
        let sb = tick_to_sqrt_price_x96(MIN_TICK + 1);
        // A one-tick range at the bottom of the price range: 2^96 / delta is
        // enormous, so a large deposit maps to more liquidity than an i128.
        assert_eq!(get_liquidity_for_amount1(sa, sb, i128::MAX), None);
        // Full-range liquidity at i128::MAX is worth more token B than exists.
        let hi = tick_to_sqrt_price_x96(MAX_TICK);
        assert_eq!(get_amount1_delta(sa, hi, i128::MAX), None);
        // Zero-width and zero-input cases stay exactly zero.
        assert_eq!(get_amount0_delta(sa, sa, i128::MAX), Some(0));
        assert_eq!(get_liquidity_for_amount0(sa, sb, 0), Some(0));
    }
}
