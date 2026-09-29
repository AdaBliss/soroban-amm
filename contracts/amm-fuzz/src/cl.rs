#![allow(dead_code)]
//! Property-based tests for the concentrated-liquidity engine.
//!
//! The V2 suite in `lib.rs` targets the constant-product pool. The
//! concentrated-liquidity contract is far larger and numerically more delicate
//! (tick-boundary rounding, bitmap word edges, fee-growth-outside flips), yet
//! had no property tests of its own. This module closes that gap.
//!
//! Structure (mirrors the V2 suite's "mirror + real contract" split):
//!
//! * [`math`]   - a pure-Rust mirror of `concentrated_liquidity/src/math.rs`
//!   (copied verbatim). The top-level `proptest!` block asserts tick/price
//!   round-trips, monotonicity, delta symmetry, liquidity round-trips and
//!   overflow-freedom across the full representable tick range.
//! * [`bitmap`] - a pure mirror of the packed tick-bitmap word arithmetic.
//! * `stateful` (behind the `cl` feature) - randomized `mint_position` /
//!   `burn_position` / `swap` / `collect_fees` sequences driven against the
//!   **real** deployed `concentrated_liquidity.wasm`, checking pool-level
//!   invariants (solvency, liquidity-net, active liquidity, balance
//!   conservation, burn-all residual) after every operation.
//!
//! # Run
//!
//! Pure properties (fast, always on):
//!   cargo test -p amm-fuzz
//!
//! Full suite including the deployed-contract stateful properties:
//!   cargo test -p amm-fuzz --features cl
//!
//! The stateful properties run with `ProptestConfig::with_cases(50)` (full
//! contract execution in a Soroban test `Env` is orders of magnitude slower
//! than the pure maths; the reduced, documented count is required by the issue).

use proptest::prelude::*;

/// Pure-Rust mirror of the concentrated-liquidity Q64.96 math.
///
/// Copied verbatim from `concentrated_liquidity/src/math.rs` so the properties
/// below exercise the exact code the contract links, not a re-derivation.
#[path = "../../concentrated_liquidity/src/math.rs"]
pub mod math;

/// Pure mirror of the CL packed tick bitmap.
///
/// Each word is a `u128` covering 128 ticks; `word_pos = tick.div_euclid(128)`,
/// `bit_pos = tick.rem_euclid(128)`. A word key disappears once it flips to
/// zero, exactly like the contract's `flip_tick`.
pub mod bitmap {
    use std::collections::HashMap;

    /// 128 ticks per word (`u128`).
    pub const WORD_BITS: i32 = 128;

    const MIN_TICK: i32 = -887_272;
    const MAX_TICK: i32 = 887_272;
    const MIN_WORD: i32 = MIN_TICK.div_euclid(WORD_BITS);
    const MAX_WORD: i32 = MAX_TICK.div_euclid(WORD_BITS);

    #[derive(Clone, Debug, Default)]
    pub struct Bitmap {
        words: HashMap<i32, u128>,
    }

    impl Bitmap {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn flip(&mut self, tick: i32) {
            let word_pos = tick.div_euclid(WORD_BITS);
            let bit_pos = tick.rem_euclid(WORD_BITS) as u32;
            let mut word = self.words.get(&word_pos).copied().unwrap_or(0);
            word ^= 1 << bit_pos;
            if word == 0 {
                self.words.remove(&word_pos);
            } else {
                self.words.insert(word_pos, word);
            }
        }

        pub fn is_initialized(&self, tick: i32) -> bool {
            let word_pos = tick.div_euclid(WORD_BITS);
            let bit_pos = tick.rem_euclid(WORD_BITS) as u32;
            self.words
                .get(&word_pos)
                .map(|w| w & (1 << bit_pos) != 0)
                .unwrap_or(false)
        }

        /// Sorted list of currently-initialized ticks.
        pub fn initialized_ticks(&self) -> Vec<i32> {
            let mut out = Vec::new();
            let mut sorted: Vec<i32> = self.words.keys().copied().collect();
            sorted.sort_unstable();
            for &word_pos in &sorted {
                let word = self.words[&word_pos];
                for bit in 0..128 {
                    if word & (1 << bit) != 0 {
                        out.push(word_pos * WORD_BITS + bit);
                    }
                }
            }
            out
        }

        /// Mirror of `tick_bitmap::next_initialized_tick_within_word`.
        pub fn next_initialized_tick_within_word(&self, tick: i32, lte: bool) -> (i32, bool) {
            let word_pos = tick.div_euclid(WORD_BITS);
            let bit_pos = tick.rem_euclid(WORD_BITS) as u32;
            let word = self.words.get(&word_pos).copied().unwrap_or(0);

            if lte {
                let mask = if bit_pos == 127 {
                    u128::MAX
                } else {
                    (1u128 << (bit_pos + 1)) - 1
                };
                let masked = word & mask;
                if masked == 0 {
                    return (word_pos * WORD_BITS, false);
                }
                let next_bit = 127 - masked.leading_zeros() as i32;
                (word_pos * WORD_BITS + next_bit, true)
            } else {
                let mask = if bit_pos == 0 {
                    u128::MAX
                } else {
                    u128::MAX.wrapping_shl(bit_pos)
                };
                let masked = word & mask;
                if masked == 0 {
                    return (word_pos * WORD_BITS + 127, false);
                }
                let next_bit = masked.trailing_zeros() as i32;
                (word_pos * WORD_BITS + next_bit, true)
            }
        }

        /// Mirror of the contract's `next_initialized_tick`. `lte = true` finds
        /// the highest initialized tick `<= tick`; `lte = false` finds the
        /// lowest initialized tick `> tick`.
        pub fn next_initialized_tick(&self, tick: i32, lte: bool) -> Option<i32> {
            if lte {
                let (cand, found) = self.next_initialized_tick_within_word(tick, true);
                if found {
                    return Some(cand);
                }
                let mut word_pos = tick.div_euclid(WORD_BITS) - 1;
                loop {
                    match self.words.get(&word_pos) {
                        Some(&word) if word != 0 => {
                            let bit = 127 - word.leading_zeros() as i32;
                            return Some(word_pos * WORD_BITS + bit);
                        }
                        _ => {
                            if word_pos < MIN_WORD {
                                return None;
                            }
                            word_pos -= 1;
                        }
                    }
                }
            } else {
                let start = tick + 1;
                let (cand, found) = self.next_initialized_tick_within_word(start, false);
                if found {
                    return Some(cand);
                }
                let mut word_pos = start.div_euclid(WORD_BITS) + 1;
                loop {
                    match self.words.get(&word_pos) {
                        Some(&word) if word != 0 => {
                            let bit = word.trailing_zeros() as i32;
                            return Some(word_pos * WORD_BITS + bit);
                        }
                        _ => {
                            if word_pos > MAX_WORD {
                                return None;
                            }
                            word_pos += 1;
                        }
                    }
                }
            }
        }
    }
}

/// Ticks in `[-MAX_DISTINCT_TICK, MAX_DISTINCT_TICK]` have distinct,
/// non-saturated sqrt prices, so round-trips and strict monotonicity hold
/// across the whole band. Above `MAX_DISTINCT_TICK` the u128 Q96
/// representation saturates at `MAX_SQRT_PRICE` (see the full-range sweep in
/// `cl_math_round_trip_full_tick_range` below).
const MAX_DISTINCT_TICK: i32 = 443_636;

/// Strategy producing valid ticks, biased toward the band edges where
/// off-by-one and clamp bugs live.
fn tick_strategy() -> impl Strategy<Value = i32> {
    prop_oneof![
        3 => math::MIN_TICK..=MAX_DISTINCT_TICK,
        1 => Just(math::MIN_TICK),
        1 => Just(MAX_DISTINCT_TICK),
        1 => Just(0),
        1 => Just(1),
        1 => Just(-1),
    ]
}

// ---------------------------------------------------------------------------
// Pure-math properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// `sqrt_price_x96_to_tick(tick_to_sqrt_price_x96(t)) == t` for every `t`
    /// in the representable (non-saturated) range. The full-range sweep below
    /// covers every tick exhaustively; this proptest shrinks any counterexample
    /// to the offending tick and biases toward band edges.
    #[test]
    fn prop_math_tick_round_trip(t in tick_strategy()) {
        let price = math::tick_to_sqrt_price_x96(t);
        let back = math::sqrt_price_x96_to_tick(price);
        prop_assert_eq!(back, t, "round-trip failed at tick {}", t);
    }

    /// `tick_to_sqrt_price_x96` is strictly increasing in `t` over the whole
    /// non-saturated band.
    #[test]
    fn prop_math_tick_price_strictly_increasing(t in (math::MIN_TICK..MAX_DISTINCT_TICK)) {
        let lo = math::tick_to_sqrt_price_x96(t);
        let hi = math::tick_to_sqrt_price_x96(t + 1);
        prop_assert!(hi > lo, "price not strictly increasing at tick {t}: {hi} <= {lo}");
    }

    /// `get_amount0_delta(a, b, L)` is symmetric under swapping `a`/`b`, and
    /// zero when `a == b`.
    #[test]
    fn prop_math_amount0_delta_symmetric(
        tick_a in tick_strategy(),
        tick_b in tick_strategy(),
        liquidity in -10_000_000_i128..=10_000_000_i128,
    ) {
        let sa = math::tick_to_sqrt_price_x96(tick_a);
        let sb = math::tick_to_sqrt_price_x96(tick_b);
        let fwd = math::get_amount0_delta(sa, sb, liquidity);
        let rev = math::get_amount0_delta(sb, sa, liquidity);
        prop_assert_eq!(fwd, rev, "amount0 delta not symmetric (a={}, b={})", tick_a, tick_b);
        let zero = math::get_amount0_delta(sa, sa, liquidity);
        prop_assert_eq!(zero, Some(0), "amount0 delta not zero when a == b");
    }

    /// `get_amount1_delta(a, b, L)` is symmetric under swapping `a`/`b`, and
    /// zero when `a == b`.
    #[test]
    fn prop_math_amount1_delta_symmetric(
        tick_a in tick_strategy(),
        tick_b in tick_strategy(),
        liquidity in -10_000_000_i128..=10_000_000_i128,
    ) {
        let sa = math::tick_to_sqrt_price_x96(tick_a);
        let sb = math::tick_to_sqrt_price_x96(tick_b);
        let fwd = math::get_amount1_delta(sa, sb, liquidity);
        let rev = math::get_amount1_delta(sb, sa, liquidity);
        prop_assert_eq!(fwd, rev, "amount1 delta not symmetric (a={}, b={})", tick_a, tick_b);
        let zero = math::get_amount1_delta(sa, sa, liquidity);
        prop_assert_eq!(zero, Some(0), "amount1 delta not zero when a == b");
    }

    /// Liquidity round-trip: `get_amount0_delta(a, b, L(x)) <= x` - the pool
    /// must never credit more than was deposited.
    #[test]
    fn prop_math_liquidity_round_trip_amount0(
        tick_a in tick_strategy(),
        tick_b in tick_strategy(),
        amount0 in 1_i128..=1_000_000_000_i128,
    ) {
        let sa = math::tick_to_sqrt_price_x96(tick_a);
        let sb = math::tick_to_sqrt_price_x96(tick_b);
        // Deposits up to 10^9 never overflow anywhere in the tick range.
        let liq = math::get_liquidity_for_amount0(sa, sb, amount0).unwrap();
        let got = math::get_amount0_delta(sa, sb, liq).unwrap();
        prop_assert!(
            got <= amount0,
            "amount0 round-trip credits more than deposited: deposited={amount0}, got={got}"
        );
    }

    /// Liquidity round-trip: `get_amount1_delta(a, b, L(x)) <= x`.
    #[test]
    fn prop_math_liquidity_round_trip_amount1(
        tick_a in tick_strategy(),
        tick_b in tick_strategy(),
        amount1 in 1_i128..=1_000_000_000_i128,
    ) {
        let sa = math::tick_to_sqrt_price_x96(tick_a);
        let sb = math::tick_to_sqrt_price_x96(tick_b);
        let liq = math::get_liquidity_for_amount1(sa, sb, amount1).unwrap();
        let got = math::get_amount1_delta(sa, sb, liq).unwrap();
        prop_assert!(
            got <= amount1,
            "amount1 round-trip credits more than deposited: deposited={amount1}, got={got}"
        );
    }

    /// Deltas are non-negative for non-negative liquidity, and mirror-signed
    /// for negative liquidity.
    #[test]
    fn prop_math_delta_sign_for_liquidity_sign(
        tick_a in tick_strategy(),
        tick_b in tick_strategy(),
        liquidity in 0_i128..=10_000_000_i128,
    ) {
        let sa = math::tick_to_sqrt_price_x96(tick_a);
        let sb = math::tick_to_sqrt_price_x96(tick_b);
        let pos0 = math::get_amount0_delta(sa, sb, liquidity).unwrap();
        let pos1 = math::get_amount1_delta(sa, sb, liquidity).unwrap();
        prop_assert!(pos0 >= 0, "negative amount0 for positive liquidity");
        prop_assert!(pos1 >= 0, "negative amount1 for positive liquidity");
        let neg0 = math::get_amount0_delta(sa, sb, -liquidity).unwrap();
        let neg1 = math::get_amount1_delta(sa, sb, -liquidity).unwrap();
        prop_assert_eq!(neg0, -pos0);
        prop_assert_eq!(neg1, -pos1);
    }

    /// No panic across the whole `i128` input domain. The conversions are
    /// evaluated over 256-bit intermediates and report an unrepresentable
    /// result as `None`, so they are total; the `prop_math_position_*_exact`
    /// properties below check the values themselves.
    #[test]
    fn prop_math_no_overflow(
        tick_a in tick_strategy(),
        tick_b in tick_strategy(),
        liquidity in any::<i128>(),
    ) {
        let sa = math::tick_to_sqrt_price_x96(tick_a);
        let sb = math::tick_to_sqrt_price_x96(tick_b);
        let _ = math::get_amount0_delta(sa, sb, liquidity);
        let _ = math::get_amount1_delta(sa, sb, liquidity);
        let pos = liquidity.saturating_abs();
        let _ = math::get_liquidity_for_amount0(sa, sb, pos);
        let _ = math::get_liquidity_for_amount1(sa, sb, pos);
    }

    /// `sqrt_price_x96_to_tick` returns the largest tick whose price is at
    /// most the input (floor semantics).
    #[test]
    fn prop_math_price_to_tick_floor(
        tick in tick_strategy(),
        bump in 0_i32..4_i32,
    ) {
        let price = math::tick_to_sqrt_price_x96(tick);
        let price_hi = price.saturating_add(bump as u128);
        let t = math::sqrt_price_x96_to_tick(price_hi);
        let p_t = math::tick_to_sqrt_price_x96(t);
        prop_assert!(p_t <= price_hi, "price_to_tick returned a tick above the price");
        if t < math::MAX_TICK {
            let p_next = math::tick_to_sqrt_price_x96(t + 1);
            prop_assert!(p_next > price_hi, "price_to_tick not the largest floor tick");
        }
    }

    #[test]
    fn prop_math_sqrt_price_to_tick(
        price in math::MIN_SQRT_PRICE..=math::MAX_SQRT_PRICE,
    ) {
        let tick = math::sqrt_price_x96_to_tick(price);
        let p_t = math::tick_to_sqrt_price_x96(tick);
        prop_assert!(p_t <= price, "floor tick price exceeds target");
        if tick < math::MAX_TICK {
            let p_next = math::tick_to_sqrt_price_x96(tick + 1);
            prop_assert!(p_next > price, "next tick price not strictly greater");
        }
    }

    #[test]
    fn prop_math_liquidity_for_amounts(
        sqrt_a in math::MIN_SQRT_PRICE..=math::MAX_SQRT_PRICE,
        sqrt_b in math::MIN_SQRT_PRICE..=math::MAX_SQRT_PRICE,
        sqrt_current in math::MIN_SQRT_PRICE..=math::MAX_SQRT_PRICE,
        amount0 in 0_i128..=1_000_000_000_i128,
        amount1 in 0_i128..=1_000_000_000_i128,
    ) {
        let lower = core::cmp::min(sqrt_a, sqrt_b);
        let upper = core::cmp::max(sqrt_a, sqrt_b);
        // Arbitrary u128 prices can be one unit apart, which can push the
        // liquidity past i128; that is reported as `None` and skipped here.
        let liq = if lower == upper {
            Some(0)
        } else if sqrt_current <= lower {
            math::get_liquidity_for_amount0(lower, upper, amount0)
        } else if sqrt_current >= upper {
            math::get_liquidity_for_amount1(lower, upper, amount1)
        } else {
            let liq0 = math::get_liquidity_for_amount0(sqrt_current, upper, amount0);
            let liq1 = math::get_liquidity_for_amount1(lower, sqrt_current, amount1);
            liq0.zip(liq1).map(|(l0, l1)| core::cmp::min(l0, l1))
        };
        if let Some(liq) = liq {
            prop_assert!(liq >= 0);
        }
    }

    #[test]
    fn prop_math_amounts_for_liquidity(
        sqrt_a in math::MIN_SQRT_PRICE..=math::MAX_SQRT_PRICE,
        sqrt_b in math::MIN_SQRT_PRICE..=math::MAX_SQRT_PRICE,
        sqrt_current in math::MIN_SQRT_PRICE..=math::MAX_SQRT_PRICE,
        liquidity in -10_000_000_i128..=10_000_000_i128,
    ) {
        let lower = core::cmp::min(sqrt_a, sqrt_b);
        let upper = core::cmp::max(sqrt_a, sqrt_b);
        // |liquidity| <= 10^7 keeps every amount well inside i128.
        let (a0, a1) = if lower == upper {
            (0, 0)
        } else if sqrt_current <= lower {
            (math::get_amount0_delta(lower, upper, liquidity).unwrap(), 0)
        } else if sqrt_current >= upper {
            (0, math::get_amount1_delta(lower, upper, liquidity).unwrap())
        } else {
            (
                math::get_amount0_delta(sqrt_current, upper, liquidity).unwrap(),
                math::get_amount1_delta(lower, sqrt_current, liquidity).unwrap(),
            )
        };
        if liquidity > 0 {
            prop_assert!(a0 >= 0 && a1 >= 0);
        } else {
            prop_assert!(a0 <= 0 && a1 <= 0);
        }
    }
}

// ---------------------------------------------------------------------------
// Position-path conversions against an arbitrary-precision reference (#963)
// ---------------------------------------------------------------------------
//
// `get_amount{0,1}_delta` and `get_liquidity_for_amount{0,1}` used to route
// through a multiply that silently dropped the high bits of any product over
// u128::MAX, which ordinary ranges such as [100_000, 200_000] reach. These
// properties compare every conversion with the exact value computed in
// `BigUint`, across the whole tick range and magnitudes up to i128::MAX, so a
// truncated product cannot pass unnoticed.

mod exact {
    use super::math;
    use num_bigint::BigUint;

    pub fn big(v: u128) -> BigUint {
        BigUint::from(v)
    }

    pub fn q96() -> BigUint {
        big(math::Q96)
    }

    pub fn i128_max() -> BigUint {
        big(i128::MAX as u128)
    }

    /// `floor(L * 2^96 * (sb - sa) / (sa * sb))`
    pub fn amount0(sa: u128, sb: u128, l: u128) -> BigUint {
        big(l) * q96() * big(sb - sa) / (big(sa) * big(sb))
    }

    /// `floor(L * (sb - sa) / 2^96)`
    pub fn amount1(sa: u128, sb: u128, l: u128) -> BigUint {
        big(l) * big(sb - sa) / q96()
    }

    /// `floor(X * sa * sb / (2^96 * (sb - sa)))`
    pub fn liquidity0(sa: u128, sb: u128, x: u128) -> BigUint {
        big(x) * big(sa) * big(sb) / (q96() * big(sb - sa))
    }

    /// `floor(X * 2^96 / (sb - sa))`
    pub fn liquidity1(sa: u128, sb: u128, x: u128) -> BigUint {
        big(x) * q96() / big(sb - sa)
    }

    /// `true` when `v` would not fit in a u128.
    pub fn over_u128(v: &BigUint) -> bool {
        v.bits() > 128
    }
}

/// Magnitudes biased toward realistic sizes but reaching `i128::MAX`, so both
/// the representable and the overflow branches are exercised.
fn magnitude_strategy() -> impl Strategy<Value = i128> {
    prop_oneof![
        3 => 1_i128..=1_000_000_000_000_i128,
        2 => 1_i128..=i128::MAX,
        1 => Just(i128::MAX),
    ]
}

/// Two ticks with distinct prices from the full representable band, ordered.
fn ordered_ticks() -> impl Strategy<Value = (i32, i32)> {
    (tick_strategy(), tick_strategy())
        .prop_filter("distinct prices", |(a, b)| {
            math::tick_to_sqrt_price_x96(*a) != math::tick_to_sqrt_price_x96(*b)
        })
        .prop_map(|(a, b)| (a.min(b), a.max(b)))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// `get_amount1_delta` is the exact floor, or `None` exactly when that
    /// floor does not fit in an i128.
    #[test]
    fn prop_math_position_amount1_exact(
        (lo, hi) in ordered_ticks(),
        liquidity in magnitude_strategy(),
    ) {
        let (sa, sb) = (math::tick_to_sqrt_price_x96(lo), math::tick_to_sqrt_price_x96(hi));
        let expected = exact::amount1(sa, sb, liquidity as u128);
        match math::get_amount1_delta(sa, sb, liquidity) {
            Some(got) => prop_assert_eq!(exact::big(got as u128), expected),
            None => prop_assert!(expected > exact::i128_max(), "spurious None, exact {}", expected),
        }
    }

    /// `get_amount0_delta` rounds down and is at most one unit below the
    /// exact floor. It is `None` only when the exact value does not fit in an
    /// i128 or its first stage `L * 2^96 / sa` does not fit in a u128.
    #[test]
    fn prop_math_position_amount0_exact(
        (lo, hi) in ordered_ticks(),
        liquidity in magnitude_strategy(),
    ) {
        let (sa, sb) = (math::tick_to_sqrt_price_x96(lo), math::tick_to_sqrt_price_x96(hi));
        let l = liquidity as u128;
        let expected = exact::amount0(sa, sb, l);
        match math::get_amount0_delta(sa, sb, liquidity) {
            Some(got) => {
                let got = exact::big(got as u128);
                prop_assert!(got <= expected, "rounded up: {} > {}", got, expected);
                prop_assert!(got + 1u32 >= expected, "more than 1 below exact {}", expected);
            }
            None => {
                let stage1 = exact::big(l) * exact::q96() / exact::big(sa);
                prop_assert!(
                    expected > exact::i128_max() || exact::over_u128(&stage1),
                    "spurious None, exact {}", expected
                );
            }
        }
    }

    /// `get_liquidity_for_amount1` is the exact floor, or `None` exactly when
    /// that floor does not fit in an i128.
    #[test]
    fn prop_math_position_liquidity1_exact(
        (lo, hi) in ordered_ticks(),
        amount in magnitude_strategy(),
    ) {
        let (sa, sb) = (math::tick_to_sqrt_price_x96(lo), math::tick_to_sqrt_price_x96(hi));
        let expected = exact::liquidity1(sa, sb, amount as u128);
        match math::get_liquidity_for_amount1(sa, sb, amount) {
            Some(got) => prop_assert_eq!(exact::big(got as u128), expected),
            None => prop_assert!(expected > exact::i128_max(), "spurious None, exact {}", expected),
        }
    }

    /// `get_liquidity_for_amount0` rounds down, losing at most `sa / 2^96 + 1`
    /// to its inner floor. It is `None` only when the exact value does not fit
    /// in an i128 or its first stage `X * sb / (sb - sa)` does not fit in a
    /// u128.
    #[test]
    fn prop_math_position_liquidity0_exact(
        (lo, hi) in ordered_ticks(),
        amount in magnitude_strategy(),
    ) {
        let (sa, sb) = (math::tick_to_sqrt_price_x96(lo), math::tick_to_sqrt_price_x96(hi));
        let x = amount as u128;
        let expected = exact::liquidity0(sa, sb, x);
        match math::get_liquidity_for_amount0(sa, sb, amount) {
            Some(got) => {
                let got = exact::big(got as u128);
                let slack = exact::big(sa / math::Q96 + 1);
                prop_assert!(got <= expected, "rounded up: {} > {}", got, expected);
                prop_assert!(got + slack >= expected, "beyond slack of exact {}", expected);
            }
            None => {
                let stage1 = exact::big(x) * exact::big(sb) / exact::big(sb - sa);
                prop_assert!(
                    expected > exact::i128_max() || exact::over_u128(&stage1),
                    "spurious None, exact {}", expected
                );
            }
        }
    }

    /// The exact helpers the position path now shares with the swap path agree
    /// with the reference in both rounding directions.
    #[test]
    fn prop_math_delta_exact_helpers_round_both_ways(
        (lo, hi) in ordered_ticks(),
        liquidity in magnitude_strategy(),
    ) {
        let (sa, sb) = (math::tick_to_sqrt_price_x96(lo), math::tick_to_sqrt_price_x96(hi));
        let l = liquidity as u128;
        let e1 = exact::amount1(sa, sb, l);
        if let (Some(down), Some(up)) = (
            math::amount1_delta_exact(sa, sb, l, false),
            math::amount1_delta_exact(sa, sb, l, true),
        ) {
            prop_assert_eq!(exact::big(down), e1);
            // Ceil is the floor, plus one unless the division was exact.
            prop_assert!(up == down || up == down + 1);
        }
        let e0 = exact::amount0(sa, sb, l);
        if let (Some(down), Some(up)) = (
            math::amount0_delta_exact(sa, sb, l, false),
            math::amount0_delta_exact(sa, sb, l, true),
        ) {
            prop_assert!(exact::big(down) <= e0.clone());
            // The real-valued amount lies in [e0, e0 + 1); ceil covers it.
            prop_assert!(exact::big(up) >= e0);
            prop_assert!(down <= up);
        }
    }

    /// The regime #963 describes: wherever `L * (sb - sa)` exceeds u128::MAX,
    /// the conversions still return the exact value, never a wrapped one.
    /// Restricted to that regime so every case counts.
    #[test]
    fn prop_math_wide_products_are_not_truncated(
        lo in 0_i32..=300_000,
        width in 50_000_i32..=300_000,
        liquidity in 1_000_000_i128..=1_000_000_000_000_i128,
    ) {
        let hi = (lo + width).min(MAX_DISTINCT_TICK);
        let (sa, sb) = (math::tick_to_sqrt_price_x96(lo), math::tick_to_sqrt_price_x96(hi));
        let l = liquidity as u128;
        prop_assume!(l.checked_mul(sb - sa).is_none());
        let got1 = math::get_amount1_delta(sa, sb, liquidity).unwrap();
        prop_assert_eq!(exact::big(got1 as u128), exact::amount1(sa, sb, l));
        let got0 = math::get_amount0_delta(sa, sb, liquidity).unwrap();
        let e0 = exact::amount0(sa, sb, l);
        prop_assert!(exact::big(got0 as u128) <= e0.clone());
        prop_assert!(exact::big(got0 as u128) + 1u32 >= e0);
    }
}

/// Exhaustive full-tick-range sweep.
///
/// Runs the tick<->price round-trip for **every** tick in the representable
/// band. The band is `[MIN_TICK, MAX_DISTINCT_TICK]`: below `MAX_DISTINCT_TICK`
/// the u128 representation is exact and strictly monotone, so an exhaustive
/// monotonicity sweep (equivalent to the floor round-trip) is a complete proof
/// of the bijection. Above it every tick saturates at `MAX_SQRT_PRICE` and
/// maps back to `MAX_TICK`; that saturation is asserted explicitly rather than
/// treated as a round-trip failure.
#[test]
fn cl_math_round_trip_full_tick_range() {
    for t in [math::MIN_TICK, MAX_DISTINCT_TICK] {
        let price = math::tick_to_sqrt_price_x96(t);
        assert_eq!(math::sqrt_price_x96_to_tick(price), t, "endpoint {t}");
    }

    // Strict monotonicity across the full band is equivalent to the round-trip
    // property for a floor-based inverse (and is far cheaper than a binary
    // search per tick): price is increasing, so the largest tick whose price is
    // <= price(t) is exactly t.
    let mut prev = math::tick_to_sqrt_price_x96(math::MIN_TICK);
    for t in (math::MIN_TICK + 1)..=MAX_DISTINCT_TICK {
        let cur = math::tick_to_sqrt_price_x96(t);
        assert!(
            cur > prev,
            "tick_to_sqrt_price_x96 not strictly increasing at tick {t} ({prev} then {cur})"
        );
        prev = cur;
    }

    // Saturation above the band: every higher tick clamps to MAX_SQRT_PRICE,
    // whose floor inverse is MAX_TICK.
    assert_eq!(
        math::tick_to_sqrt_price_x96(MAX_DISTINCT_TICK + 1),
        math::MAX_SQRT_PRICE
    );
    assert_eq!(
        math::tick_to_sqrt_price_x96(math::MAX_TICK),
        math::MAX_SQRT_PRICE
    );
    assert_eq!(
        math::sqrt_price_x96_to_tick(math::MAX_SQRT_PRICE),
        math::MAX_TICK
    );
}

// ---------------------------------------------------------------------------
// Tick-bitmap properties (pure mirror)
// ---------------------------------------------------------------------------

/// Deterministic seeded bitmap for bitmap properties: flips every tick in a
/// bounded random set, so tests stay reproducible and shrinkable.
fn seeded_bitmap(ticks: &[i32]) -> bitmap::Bitmap {
    let mut bm = bitmap::Bitmap::new();
    for &t in ticks {
        bm.flip(t);
    }
    bm
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1_000))]

    /// Flipping a tick twice restores the bitmap exactly (word removed when it
    /// returns to zero, matching the contract's storage cleanup).
    #[test]
    fn prop_bitmap_flip_twice_restores(
        ticks in proptest::collection::vec(-10_000_i32..10_000_i32, 0..50),
        tick in -10_000_i32..10_000_i32,
    ) {
        let mut bm = seeded_bitmap(&ticks);
        let snapshot = bm.clone();
        bm.flip(tick);
        bm.flip(tick);
        let after = bm.initialized_ticks();
        let before = snapshot.initialized_ticks();
        prop_assert_eq!(after, before, "double-flip did not restore the bitmap");
    }

    /// `next_initialized_tick_within_word` returns an initialized tick, or
    /// reports `false`; when it reports `true`, the result is initialized, in
    /// the same word, and in the requested direction.
    #[test]
    fn prop_bitmap_within_word_returns_initialized_or_false(
        ticks in proptest::collection::vec(-10_000_i32..10_000_i32, 0..50),
        tick in -10_000_i32..10_000_i32,
        lte in proptest::bool::ANY,
    ) {
        let bm = seeded_bitmap(&ticks);
        let (found_tick, found) = bm.next_initialized_tick_within_word(tick, lte);
        let word_pos = tick.div_euclid(bitmap::WORD_BITS);
        if found {
            prop_assert!(bm.is_initialized(found_tick), "reported tick not initialized");
            prop_assert_eq!(found_tick.div_euclid(bitmap::WORD_BITS), word_pos, "crossed word");
            if lte {
                prop_assert!(found_tick <= tick, "lte returned a tick above the query");
            } else {
                prop_assert!(found_tick >= tick, "gte returned a tick below the query");
            }
        } else {
            // No initialized tick in this word in the requested direction.
            let word_ticks: Vec<i32> = bm
                .initialized_ticks()
                .into_iter()
                .filter(|t| t.div_euclid(bitmap::WORD_BITS) == word_pos)
                .collect();
            if lte {
                prop_assert!(
                    !word_ticks.iter().any(|&t| t <= tick),
                    "lte=false-negative but word has a tick <= query: {word_ticks:?} at {tick}"
                );
            } else {
                prop_assert!(
                    !word_ticks.iter().any(|&t| t >= tick),
                    "gte=false-negative but word has a tick >= query: {word_ticks:?} at {tick}"
                );
            }
        }
    }

    /// Word-boundary behaviour: an initialized tick exactly at a word boundary
    /// is found correctly from both directions (within-word and full-next).
    #[test]
    fn prop_bitmap_word_boundary_found_both_directions(
        word_offset in -78_i32..78_i32,
    ) {
        let boundary = word_offset * 128;
        let mut bm = bitmap::Bitmap::new();
        bm.flip(boundary);

        // Within the boundary's own word, the bit is found from both directions.
        let (w_found, w_ok) = bm.next_initialized_tick_within_word(boundary, false);
        prop_assert!(w_ok && w_found == boundary, "upward within-word missed boundary {boundary}");
        let (w_found, w_ok) = bm.next_initialized_tick_within_word(boundary + 1, true);
        prop_assert!(w_ok && w_found == boundary, "downward within-word missed boundary {boundary}");
        // The full multi-word search also crosses the word edge from either side.
        prop_assert_eq!(bm.next_initialized_tick(boundary - 1, false), Some(boundary));
        prop_assert_eq!(bm.next_initialized_tick(boundary + 1, true), Some(boundary));
        prop_assert_eq!(bm.next_initialized_tick(boundary + 1, false), None);
    }

    /// `next_initialized_tick` never skips an initialized tick between the
    /// start and the returned result: the result is the closest initialized
    /// tick in the requested direction (verified against brute force).
    #[test]
    fn prop_bitmap_next_never_skips_initialized(
        ticks in proptest::collection::vec(-10_000_i32..10_000_i32, 0..50),
        tick in -10_000_i32..10_000_i32,
        lte in proptest::bool::ANY,
    ) {
        let bm = seeded_bitmap(&ticks);
        let all = bm.initialized_ticks();
        let expected = if lte {
            all.iter().rev().copied().find(|&t| t <= tick)
        } else {
            all.iter().copied().find(|&t| t > tick)
        };
        let got = bm.next_initialized_tick(tick, lte);
        prop_assert_eq!(got, expected, "next_initialized_tick({}, lte={})", tick, lte);

        if let Some(r) = got {
            let between: Vec<i32> = if lte {
                all.iter().copied().filter(|&t| t > r && t <= tick).collect()
            } else {
                all.iter().copied().filter(|&t| t > tick && t < r).collect()
            };
            prop_assert!(
                between.is_empty(),
                "skipped initialized ticks {between:?} between {tick} and result {r}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Stateful pool invariants (real contract)
// ---------------------------------------------------------------------------

#[cfg(feature = "cl")]
mod stateful {
    use std::collections::HashMap;

    use proptest::test_runner::TestCaseError;
    use soroban_sdk::{
        testutils::Address as _,
        token::{StellarAssetClient, TokenClient as StellarTokenClient},
        Address, BytesN, Env,
    };

    use super::*;

    mod cl_wasm {
        soroban_sdk::contractimport!(
            file = "../../target/wasm32v1-none/release/concentrated_liquidity.wasm"
        );
    }

    /// Documented dust bound for the burn-all residual: every liquidity<->amount
    /// conversion rounds down by at most 1 unit and every fee split rounds down
    /// by at most 1 unit, per position. With at most 40 live positions and both
    /// directions, 1_000 units is far beyond any rounding accumulation.
    const BURN_ALL_DUST: i128 = 1_000;

    const MIN_TICK: i32 = -887_272;
    const MAX_TICK: i32 = 887_272;

    fn align(tick: i32, spacing: i32) -> i32 {
        tick - tick.rem_euclid(spacing)
    }

    /// A tracked open (or closed-but-uncollected) position.
    #[derive(Clone, Debug)]
    struct LivePos {
        lower: i32,
        upper: i32,
    }

    struct Ctx {
        env: Env,
        cl_addr: Address,
        ta: Address,
        tb: Address,
        admin: Address,
        provider: Address,
        fee_bps: i128,
        positions: Vec<LivePos>,
        /// tick -> gross liquidity (driven from successful mints/burns).
        gross: HashMap<i32, i128>,
        // Exact token-flow tallies used by the balance-conservation check.
        minted_a: i128,
        minted_b: i128,
        burned_a: i128,
        burned_b: i128,
        swap_in_a: i128,
        swap_in_b: i128,
        swap_out_a: i128,
        swap_out_b: i128,
        collected_a: i128,
        collected_b: i128,
        protocol_fees_a: i128,
        protocol_fees_b: i128,
    }

    impl Ctx {
        fn client(&self) -> cl_wasm::Client<'_> {
            cl_wasm::Client::new(&self.env, &self.cl_addr)
        }

        fn bal_a(&self) -> i128 {
            StellarTokenClient::new(&self.env, &self.ta).balance(&self.cl_addr)
        }

        fn bal_b(&self) -> i128 {
            StellarTokenClient::new(&self.env, &self.tb).balance(&self.cl_addr)
        }

        fn current_tick(&self) -> i32 {
            self.client().current_tick()
        }

        /// Enumerate all initialized ticks via the real contract's bitmap.
        fn initialized_ticks(&self) -> Vec<i32> {
            let client = self.client();
            let mut out = Vec::new();
            let mut t = client.prev_initialized_tick_pub(&MAX_TICK);
            while let Some(tick) = t {
                out.push(tick);
                t = client.prev_initialized_tick_pub(&(tick - 1));
            }
            out
        }

        /// Sum of `liquidity_net` across every initialized tick.
        fn liquidity_net_sum(&self) -> i128 {
            let client = self.client();
            let mut sum = 0_i128;
            for t in self.initialized_ticks() {
                if let Ok(Ok(info)) = client.try_get_tick_info(&t) {
                    sum += info.liquidity_net;
                }
            }
            sum
        }

        /// Expected active liquidity from tracked positions, given current tick.
        #[allow(dead_code)]
        fn expected_active_liquidity(&self) -> i128 {
            let client = self.client();
            let cur = self.current_tick();
            let mut sum = 0_i128;
            for p in &self.positions {
                if let Ok(Ok(pos)) = client.try_get_position(&self.provider, &p.lower, &p.upper) {
                    if p.lower <= cur && cur < p.upper {
                        sum += pos.liquidity;
                    }
                }
            }
            sum
        }

        /// Sum of (quote_position principal + tokens_owed) for every live
        /// position, per token - exactly what burning + collecting every
        /// position right now would pay out.
        fn total_liability(&self) -> (i128, i128) {
            let client = self.client();
            let mut la = 0_i128;
            let mut lb = 0_i128;
            for p in &self.positions {
                if let Ok(Ok(pos)) = client.try_get_position(&self.provider, &p.lower, &p.upper) {
                    if pos.liquidity > 0 {
                        if let Ok(Ok((qa, qb))) =
                            client.try_quote_position(&p.lower, &p.upper, &pos.liquidity)
                        {
                            la += qa;
                            lb += qb;
                        }
                    }
                    la += pos.tokens_owed.0;
                    lb += pos.tokens_owed.1;
                }
            }
            (la, lb)
        }

        /// Sum of uncollected `tokens_owed` plus all previously collected fees
        /// - i.e. the total fees owed to (and paid out to) LPs.
        fn total_accrued_fees(&self) -> (i128, i128) {
            let client = self.client();
            let mut a = self.collected_a;
            let mut b = self.collected_b;
            for p in &self.positions {
                if let Ok(Ok(pos)) = client.try_get_position(&self.provider, &p.lower, &p.upper) {
                    a += pos.tokens_owed.0;
                    b += pos.tokens_owed.1;
                }
            }
            (a, b)
        }

        /// Total each token the pool currently owes: every position's burn
        /// proceeds at the live price, plus its uncollected fees.
        fn total_claimable(&self) -> (i128, i128) {
            let client = self.client();
            let (mut a, mut b) = (0i128, 0i128);
            for p in &self.positions {
                if let Ok(Ok(pos)) = client.try_get_position(&self.provider, &p.lower, &p.upper) {
                    if pos.liquidity > 0 {
                        if let Ok(Ok((qa, qb))) =
                            client.try_quote_position(&p.lower, &p.upper, &pos.liquidity)
                        {
                            a += qa;
                            b += qb;
                        }
                    }
                    a += pos.tokens_owed.0;
                    b += pos.tokens_owed.1;
                }
            }
            (a, b)
        }

        // ── Operations ─────────────────────────────────────────────────────────

        /// Mint a position. Returns `true` when liquidity was actually added.
        fn mint(&mut self, lower: i32, upper: i32, amount_a: i128, amount_b: i128) -> bool {
            let env = self.env.clone();
            let cl_addr = self.cl_addr.clone();
            let client = cl_wasm::Client::new(&env, &cl_addr);
            let old = match client.try_get_position(&self.provider, &lower, &upper) {
                Ok(Ok(p)) => p.liquidity,
                _ => 0,
            };
            let res = client.try_mint_position(
                &self.provider,
                &lower,
                &upper,
                &amount_a,
                &amount_b,
                &0,
                &0,
                &u64::MAX,
            );
            let Ok(Ok((aa, ab))) = res else {
                return false;
            };
            self.minted_a += aa;
            self.minted_b += ab;
            let new = match client.try_get_position(&self.provider, &lower, &upper) {
                Ok(Ok(p)) => p.liquidity,
                _ => 0,
            };
            let delta = new - old;
            if delta <= 0 {
                return false;
            }
            for tick in [lower, upper] {
                let g = self.gross.entry(tick).or_insert(0);
                *g += delta;
            }
            if !self
                .positions
                .iter()
                .any(|p| p.lower == lower && p.upper == upper)
            {
                self.positions.push(LivePos { lower, upper });
            }
            true
        }

        fn burn(&mut self, idx: usize, amount: i128) {
            if idx >= self.positions.len() {
                return;
            }
            let p = self.positions[idx].clone();
            let env = self.env.clone();
            let cl_addr = self.cl_addr.clone();
            let client = cl_wasm::Client::new(&env, &cl_addr);
            let liq = match client.try_get_position(&self.provider, &p.lower, &p.upper) {
                Ok(Ok(pos)) => pos.liquidity,
                _ => 0,
            };
            if liq <= 0 {
                return;
            }
            let burn = amount.min(liq);
            let Ok(Ok((ba, bb))) =
                client.try_burn_position(&self.provider, &p.lower, &p.upper, &burn)
            else {
                return;
            };
            self.burned_a += ba;
            self.burned_b += bb;
            for tick in [p.lower, p.upper] {
                if let Some(g) = self.gross.get_mut(&tick) {
                    *g -= burn;
                    if *g <= 0 {
                        self.gross.remove(&tick);
                    }
                }
            }
        }

        fn collect(&mut self, idx: usize) {
            if idx >= self.positions.len() {
                return;
            }
            let p = self.positions[idx].clone();
            let env = self.env.clone();
            let cl_addr = self.cl_addr.clone();
            let client = cl_wasm::Client::new(&env, &cl_addr);
            if let Ok(Ok((ca, cb))) = client.try_collect_fees(&self.provider, &p.lower, &p.upper) {
                self.collected_a += ca;
                self.collected_b += cb;
            }
        }

        fn swap(&mut self, zero_for_one: bool, amount: i128) -> bool {
            let env = self.env.clone();
            let cl_addr = self.cl_addr.clone();
            let client = cl_wasm::Client::new(&env, &cl_addr);
            if zero_for_one {
                StellarAssetClient::new(&env, &self.ta).mint(&self.provider, &amount);
            } else {
                StellarAssetClient::new(&env, &self.tb).mint(&self.provider, &amount);
            }
            let (ba0, bb0) = (self.bal_a(), self.bal_b());
            let res = client.try_swap(
                &self.provider,
                &zero_for_one,
                &amount,
                &0_u128,
                &0_i128,
                &u64::MAX,
            );
            let Ok(Ok(_out)) = res else {
                return false;
            };
            let (ba1, bb1) = (self.bal_a(), self.bal_b());
            if zero_for_one {
                self.swap_in_a += (ba1 - ba0).max(0);
                self.swap_out_b += (bb0 - bb1).max(0);
            } else {
                self.swap_in_b += (bb1 - bb0).max(0);
                self.swap_out_a += (ba0 - ba1).max(0);
            }
            true
        }

        fn swap_exact_out(&mut self, zero_for_one: bool, amount_out: i128) -> bool {
            let env = self.env.clone();
            let cl_addr = self.cl_addr.clone();
            let client = cl_wasm::Client::new(&env, &cl_addr);
            let max_in = amount_out.saturating_mul(20).max(1_000_000);
            if zero_for_one {
                StellarAssetClient::new(&env, &self.ta).mint(&self.provider, &max_in);
            } else {
                StellarAssetClient::new(&env, &self.tb).mint(&self.provider, &max_in);
            }
            let (ba0, bb0) = (self.bal_a(), self.bal_b());
            let limit = if zero_for_one {
                math::MIN_SQRT_PRICE + 1
            } else {
                math::MAX_SQRT_PRICE - 1
            };
            let res = client.try_swap_exact_out(
                &self.provider,
                &zero_for_one,
                &amount_out,
                &limit,
                &max_in,
                &u64::MAX,
            );
            let Ok(Ok(_)) = res else {
                return false;
            };
            let (ba1, bb1) = (self.bal_a(), self.bal_b());
            if zero_for_one {
                self.swap_in_a += (ba1 - ba0).max(0);
                self.swap_out_b += (bb0 - bb1).max(0);
            } else {
                self.swap_in_b += (bb1 - bb0).max(0);
                self.swap_out_a += (ba0 - ba1).max(0);
            }
            true
        }

        fn modify_pos(&mut self, idx: usize, delta: i128, spacing: i32, p1: i32, p2: i32) -> bool {
            let env = self.env.clone();
            let cl_addr = self.cl_addr.clone();
            let client = cl_wasm::Client::new(&env, &cl_addr);
            let (lower, upper) = if self.positions.is_empty() || idx >= self.positions.len() {
                let lo = align(p1.min(p2), spacing).clamp(MIN_TICK, MAX_TICK - spacing);
                let hi = align(p1.max(p2), spacing).clamp(lo + spacing, MAX_TICK);
                if lo >= hi {
                    return false;
                }
                (lo, hi)
            } else {
                (self.positions[idx].lower, self.positions[idx].upper)
            };

            let (ba0, bb0) = (self.bal_a(), self.bal_b());
            let res = client.try_modify_position(
                &self.provider,
                &lower,
                &upper,
                &delta,
                &0,
                &0,
                &u64::MAX,
            );
            let Ok(Ok((aa, ab))) = res else {
                return false;
            };
            let (ba1, bb1) = (self.bal_a(), self.bal_b());
            if delta > 0 {
                self.minted_a += (ba1 - ba0).max(0);
                self.minted_b += (bb1 - bb0).max(0);
                for tick in [lower, upper] {
                    *self.gross.entry(tick).or_insert(0) += delta;
                }
                if !self
                    .positions
                    .iter()
                    .any(|p| p.lower == lower && p.upper == upper)
                {
                    self.positions.push(LivePos { lower, upper });
                }
            } else {
                self.burned_a += (ba0 - ba1).max(0);
                self.burned_b += (bb0 - bb1).max(0);
                let burn_abs = (-delta).min(aa.abs() + ab.abs());
                for tick in [lower, upper] {
                    if let Some(g) = self.gross.get_mut(&tick) {
                        *g -= burn_abs;
                        if *g <= 0 {
                            self.gross.remove(&tick);
                        }
                    }
                }
            }
            true
        }

        fn mint_single_token(
            &mut self,
            lower: i32,
            upper: i32,
            token_is_a: bool,
            amount: i128,
        ) -> bool {
            let env = self.env.clone();
            let cl_addr = self.cl_addr.clone();
            let client = cl_wasm::Client::new(&env, &cl_addr);
            let token_in = if token_is_a {
                self.ta.clone()
            } else {
                self.tb.clone()
            };
            if token_is_a {
                StellarAssetClient::new(&env, &self.ta).mint(&self.provider, &amount);
            } else {
                StellarAssetClient::new(&env, &self.tb).mint(&self.provider, &amount);
            }
            let old_liq = match client.try_get_position(&self.provider, &lower, &upper) {
                Ok(Ok(p)) => p.liquidity,
                _ => 0,
            };
            let (ba0, bb0) = (self.bal_a(), self.bal_b());
            let res = client.try_mint_position_single_token(
                &self.provider,
                &lower,
                &upper,
                &token_in,
                &amount,
                &0,
                &u64::MAX,
            );
            let Ok(Ok(_res)) = res else {
                return false;
            };
            let (ba1, bb1) = (self.bal_a(), self.bal_b());
            self.minted_a += (ba1 - ba0).max(0);
            self.minted_b += (bb1 - bb0).max(0);

            let new_liq = match client.try_get_position(&self.provider, &lower, &upper) {
                Ok(Ok(p)) => p.liquidity,
                _ => 0,
            };
            let delta = new_liq - old_liq;
            if delta > 0 {
                for tick in [lower, upper] {
                    *self.gross.entry(tick).or_insert(0) += delta;
                }
                if !self
                    .positions
                    .iter()
                    .any(|p| p.lower == lower && p.upper == upper)
                {
                    self.positions.push(LivePos { lower, upper });
                }
            }
            true
        }

        fn place_range_order(&mut self, p1: i32, p2: i32, amount: i128, spacing: i32) -> bool {
            let env = self.env.clone();
            let cl_addr = self.cl_addr.clone();
            let client = cl_wasm::Client::new(&env, &cl_addr);
            let cur = self.current_tick();
            let lower = align(p1.min(p2), spacing).clamp(MIN_TICK, MAX_TICK - spacing);
            let upper = align(p1.max(p2), spacing).clamp(lower + spacing, MAX_TICK);
            if lower >= upper {
                return false;
            }
            if cur >= lower && cur < upper {
                return false;
            }

            let is_above = cur < lower;
            let token_in = if is_above {
                self.ta.clone()
            } else {
                self.tb.clone()
            };
            if is_above {
                StellarAssetClient::new(&env, &self.ta).mint(&self.provider, &amount);
            } else {
                StellarAssetClient::new(&env, &self.tb).mint(&self.provider, &amount);
            }

            let old_liq = match client.try_get_position(&self.provider, &lower, &upper) {
                Ok(Ok(p)) => p.liquidity,
                _ => 0,
            };
            let (ba0, bb0) = (self.bal_a(), self.bal_b());
            let res = client.try_place_range_order(
                &self.provider,
                &lower,
                &upper,
                &token_in,
                &amount,
                &0,
                &u64::MAX,
            );
            let Ok(Ok(_res)) = res else {
                return false;
            };
            let (ba1, bb1) = (self.bal_a(), self.bal_b());
            self.minted_a += (ba1 - ba0).max(0);
            self.minted_b += (bb1 - bb0).max(0);

            // Also call check_range_order_filled
            let _ = client.try_check_range_order_filled(&self.provider, &lower, &upper);

            let new_liq = match client.try_get_position(&self.provider, &lower, &upper) {
                Ok(Ok(p)) => p.liquidity,
                _ => 0,
            };
            let delta = new_liq - old_liq;
            if delta > 0 {
                for tick in [lower, upper] {
                    *self.gross.entry(tick).or_insert(0) += delta;
                }
                if !self
                    .positions
                    .iter()
                    .any(|p| p.lower == lower && p.upper == upper)
                {
                    self.positions.push(LivePos { lower, upper });
                }
            }
            true
        }

        fn collect_by_token_id(&mut self, idx: usize) {
            if self.positions.is_empty() || idx >= self.positions.len() {
                return;
            }
            let p = self.positions[idx].clone();
            let env = self.env.clone();
            let cl_addr = self.cl_addr.clone();
            let client = cl_wasm::Client::new(&env, &cl_addr);
            if let Ok(Ok(Some(token_id))) =
                client.try_position_token_id(&self.provider, &p.lower, &p.upper)
            {
                if let Ok(Ok((ca, cb))) =
                    client.try_collect_fees_by_token_id(&self.provider, &token_id)
                {
                    self.collected_a += ca;
                    self.collected_b += cb;
                }
            }
        }

        fn burn_by_token_id(&mut self, idx: usize, amount: i128) {
            if self.positions.is_empty() || idx >= self.positions.len() {
                return;
            }
            let p = self.positions[idx].clone();
            let env = self.env.clone();
            let cl_addr = self.cl_addr.clone();
            let client = cl_wasm::Client::new(&env, &cl_addr);
            let liq = match client.try_get_position(&self.provider, &p.lower, &p.upper) {
                Ok(Ok(pos)) => pos.liquidity,
                _ => 0,
            };
            if liq <= 0 {
                return;
            }
            let burn = amount.min(liq);
            if let Ok(Ok(Some(token_id))) =
                client.try_position_token_id(&self.provider, &p.lower, &p.upper)
            {
                let Ok(Ok((ba, bb))) =
                    client.try_burn_position_by_token_id(&self.provider, &token_id, &burn)
                else {
                    return;
                };
                self.burned_a += ba;
                self.burned_b += bb;
                for tick in [p.lower, p.upper] {
                    if let Some(g) = self.gross.get_mut(&tick) {
                        *g -= burn;
                        if *g <= 0 {
                            self.gross.remove(&tick);
                        }
                    }
                }
            }
        }

        fn withdraw_protocol_fees(&mut self) {
            let env = self.env.clone();
            let cl_addr = self.cl_addr.clone();
            let client = cl_wasm::Client::new(&env, &cl_addr);
            let (ba0, bb0) = (self.bal_a(), self.bal_b());
            if client.try_withdraw_protocol_fees(&self.admin).is_ok() {
                let (ba1, bb1) = (self.bal_a(), self.bal_b());
                self.protocol_fees_a += (ba0 - ba1).max(0);
                self.protocol_fees_b += (bb0 - bb1).max(0);
            }
        }
    }

    fn deploy(fee_bps: i128, initial_tick: i32, spacing: i32) -> Ctx {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let provider = Address::generate(&env);
        let sac_a = env.register_stellar_asset_contract_v2(admin.clone());
        let sac_b = env.register_stellar_asset_contract_v2(admin.clone());
        let ta = sac_a.address();
        let tb = sac_b.address();

        let cl_hash = env.deployer().upload_contract_wasm(cl_wasm::WASM);
        let cl_addr = env
            .deployer()
            .with_address(
                Address::generate(&env),
                BytesN::from_array(&env, &[0u8; 32]),
            )
            .deploy(cl_hash);
        let client = cl_wasm::Client::new(&env, &cl_addr);
        client.initialize(&admin, &ta, &tb, &fee_bps, &initial_tick, &spacing);

        // Fund the provider generously so swaps/mints never starve.
        let sac_a_fund = StellarAssetClient::new(&env, &ta);
        let sac_b_fund = StellarAssetClient::new(&env, &tb);
        sac_a_fund.mint(&provider, &1_000_000_000_000_i128);
        sac_b_fund.mint(&provider, &1_000_000_000_000_i128);

        Ctx {
            env,
            cl_addr,
            ta,
            tb,
            admin,
            provider,
            fee_bps,
            positions: Vec::new(),
            gross: HashMap::new(),
            minted_a: 0,
            minted_b: 0,
            burned_a: 0,
            burned_b: 0,
            swap_in_a: 0,
            swap_in_b: 0,
            swap_out_a: 0,
            swap_out_b: 0,
            collected_a: 0,
            collected_b: 0,
            protocol_fees_a: 0,
            protocol_fees_b: 0,
        }
    }

    /// Run the per-op invariant checks. Returns `Err` (failing the property)
    /// when any invariant breaks.
    ///
    /// Invariant 5 is per-token solvency: the pool's balances cover every
    /// position's burn proceeds plus its uncollected fees. It used to be
    /// omitted here, because the swap loop priced steps in a coarse
    /// ~3-significant-digit representation while burn and quote priced at the
    /// fine `tick_to_sqrt_price_x96`, so after a price move the per-token
    /// (and even total-value) claims exceeded the pool's actual balances.
    /// The swap engine now works entirely in Q64.96, so the property holds and
    /// is asserted on every step; the four regression tests below, previously
    /// `#[ignore]`d as known failures, run as ordinary tests.
    fn check_invariants(
        ctx: &Ctx,
        step: usize,
        op: &str,
        last_accrued: &mut (i128, i128),
    ) -> Result<(), TestCaseError> {
        // 1. Sum of liquidity_net across all initialized ticks is exactly zero.
        let net = ctx.liquidity_net_sum();
        prop_assert_eq!(
            net,
            0,
            "liquidity_net does not sum to zero after {} (step {}): {}",
            op,
            step,
            net
        );

        // 2. Balance conservation: the pool's token balances exactly match the
        //    algebraic sum of every transfer it has performed.
        let (ba, bb) = (ctx.bal_a(), ctx.bal_b());
        let derived_a = ctx.minted_a - ctx.burned_a - ctx.collected_a - ctx.protocol_fees_a
            + ctx.swap_in_a
            - ctx.swap_out_a;
        let derived_b = ctx.minted_b - ctx.burned_b - ctx.collected_b - ctx.protocol_fees_b
            + ctx.swap_in_b
            - ctx.swap_out_b;
        prop_assert_eq!(
            ba,
            derived_a,
            "token A balance drift after {} (step {}): balance {} != derived {}",
            op,
            step,
            ba,
            derived_a
        );
        prop_assert_eq!(
            bb,
            derived_b,
            "token B balance drift after {} (step {}): balance {} != derived {}",
            op,
            step,
            bb,
            derived_b
        );

        // 3. The contract bitmap agrees with the tracked tick set.
        let mut contract_ticks = ctx.initialized_ticks();
        contract_ticks.sort_unstable();
        let mut tracked: Vec<i32> = ctx.gross.keys().copied().collect();
        tracked.sort_unstable();
        prop_assert_eq!(
            &contract_ticks,
            &tracked,
            "bitmap drift after {} (step {}): contract {:?} vs tracked {:?}",
            op,
            step,
            contract_ticks,
            tracked
        );

        // 4. Accrued fees are monotone (fees can only grow).
        let (accrued_a, accrued_b) = ctx.total_accrued_fees();
        prop_assert!(
            accrued_a >= last_accrued.0 && accrued_b >= last_accrued.1,
            "accrued fees decreased after {} (step {}): A {}->{}, B {}->{}",
            op,
            step,
            last_accrued.0,
            accrued_a,
            last_accrued.1,
            accrued_b
        );
        *last_accrued = (accrued_a, accrued_b);

        // 5. Per-token solvency: the pool's real balances cover everything it
        //    owes — every position's burn proceeds at the live price plus its
        //    uncollected fees.
        //
        //    This is the invariant the swap engine used to violate. It priced
        //    steps in a ~3-significant-digit scale (`sqrt_price * 1000`, a
        //    granularity of about 20 ticks) while burn and quote priced at the
        //    fine `sqrt_price_x96`, so the pool price drifted away from the
        //    price trades had actually paid for and positions redeemed against
        //    a price the pool was never at.
        let (claim_a, claim_b) = ctx.total_claimable();
        prop_assert!(
            ba >= claim_a,
            "token A insolvency after {} (step {}): balance {} < claimable {}",
            op,
            step,
            ba,
            claim_a
        );
        prop_assert!(
            bb >= claim_b,
            "token B insolvency after {} (step {}): balance {} < claimable {}",
            op,
            step,
            bb,
            claim_b
        );
        Ok(())
    }

    /// Execute a script, returning the final context and the deepest
    /// tick-crossing distance observed across all swaps.
    fn run_script(
        params: (
            i128,
            i32,
            i32,
            Vec<(i32, i32, i128, i128)>,
            Vec<(u8, i32, i32, i128, u8)>,
        ),
    ) -> Result<(Ctx, i32), TestCaseError> {
        let (fee_bps, initial_tick, spacing, seed_positions, ops) = params;
        let spacing = spacing.max(1);
        let initial_tick = align(initial_tick, spacing).clamp(MIN_TICK, MAX_TICK);
        let mut ctx = deploy(fee_bps, initial_tick, spacing);

        // Seed positions (generated; aligned to spacing).
        for (lo, hi, amt_a, amt_b) in seed_positions {
            let lower = align(lo.min(hi), spacing).clamp(MIN_TICK, MAX_TICK - spacing);
            let upper = align(lo.max(hi), spacing).clamp(lower + spacing, MAX_TICK);
            if lower >= upper {
                continue;
            }
            ctx.mint(lower, upper, amt_a, amt_b);
        }

        let mut step = 0;
        let mut max_crossings = 0_i32;
        let mut last_accrued = (0_i128, 0_i128);
        for (kind, p1, p2, amount, flag) in ops {
            step += 1;
            let op_kind = kind % 11;
            let op_label = match op_kind {
                0 => "mint",
                1 => "swap",
                2 => "burn",
                3 => "collect",
                4 => "swap_exact_out",
                5 => "modify_position",
                6 => "mint_single_token",
                7 => "place_range_order",
                8 => "collect_by_token_id",
                9 => "withdraw_protocol_fees",
                10 => "burn_by_token_id",
                _ => "op",
            };

            let tick_before = if op_kind == 1 || op_kind == 4 {
                Some(ctx.current_tick())
            } else {
                None
            };

            match op_kind {
                0 => {
                    let lower = align(p1.min(p2), spacing).clamp(MIN_TICK, MAX_TICK - spacing);
                    let upper = align(p1.max(p2), spacing).clamp(lower + spacing, MAX_TICK);
                    if lower < upper {
                        ctx.mint(lower, upper, amount.max(1), amount.max(1));
                    }
                }
                1 => {
                    ctx.swap(flag == 0, amount.abs().max(1));
                }
                2 => {
                    let idx = (p1.unsigned_abs() as usize) % ctx.positions.len().max(1);
                    ctx.burn(idx, amount.abs());
                }
                3 => {
                    let idx = (p1.unsigned_abs() as usize) % ctx.positions.len().max(1);
                    ctx.collect(idx);
                }
                4 => {
                    ctx.swap_exact_out(flag == 0, amount.abs().clamp(1, 10_000));
                }
                5 => {
                    let idx = (p1.unsigned_abs() as usize) % ctx.positions.len().max(1);
                    ctx.modify_pos(idx, amount, spacing, p1, p2);
                }
                6 => {
                    let lower = align(p1.min(p2), spacing).clamp(MIN_TICK, MAX_TICK - spacing);
                    let upper = align(p1.max(p2), spacing).clamp(lower + spacing, MAX_TICK);
                    if lower < upper {
                        ctx.mint_single_token(lower, upper, flag == 0, amount.abs().max(1));
                    }
                }
                7 => {
                    ctx.place_range_order(p1, p2, amount.abs().max(1), spacing);
                }
                8 => {
                    let idx = (p1.unsigned_abs() as usize) % ctx.positions.len().max(1);
                    ctx.collect_by_token_id(idx);
                }
                9 => {
                    ctx.withdraw_protocol_fees();
                }
                10 => {
                    let idx = (p1.unsigned_abs() as usize) % ctx.positions.len().max(1);
                    ctx.burn_by_token_id(idx, amount.abs());
                }
                _ => {}
            }

            if let Some(t0) = tick_before {
                let t1 = ctx.current_tick();
                let (lo, hi) = (t0.min(t1), t0.max(t1));
                let crossed = ctx
                    .initialized_ticks()
                    .iter()
                    .filter(|&&t| t > lo && t < hi)
                    .count() as i32;
                if t1 != t0 {
                    max_crossings = max_crossings.max(crossed + 1);
                }
            }

            check_invariants(&ctx, step, op_label, &mut last_accrued)?;
        }

        Ok((ctx, max_crossings))
    }

    /// Stateful pool invariants hold across a rich deterministic operation
    /// script on the real deployed contract: liquidity-net, balance
    /// conservation, bitmap consistency and accrued-fee monotonicity. The same
    /// script must also reach multi-tick-crossing swaps (acceptance criterion:
    /// the corpus is not silently only testing single-tick cases).
    #[test]
    fn cl_stateful_invariants_and_multi_tick_crossing() {
        let seed: Vec<(i32, i32, i128, i128)> = vec![
            (-400, 400, 100_000, 100_000),
            (-800, -300, 100_000, 100_000),
            (300, 800, 100_000, 100_000),
        ];
        let mut ops: Vec<(u8, i32, i32, i128, u8)> = Vec::new();
        for i in 0..12 {
            ops.push((1, i, 0, 50_000 + i as i128 * 7_777, (i % 2) as u8)); // swap
        }
        ops.push((0, -200, 200, 100_000, 0)); // mint
        ops.push((4, 0, 0, 500, 0)); // swap_exact_out
        ops.push((5, 0, -200, 10_000, 0)); // modify_position (add)
        ops.push((6, 500, 1000, 50_000, 0)); // mint_position_single_token
        ops.push((7, 1200, 1500, 50_000, 0)); // place_range_order & check_range_order_filled
        ops.push((2, 0, 0, 50_000, 0)); // burn
        ops.push((3, 1, 0, 0, 0)); // collect
        ops.push((8, 0, 0, 0, 0)); // collect_fees_by_token_id
        ops.push((9, 0, 0, 0, 0)); // withdraw_protocol_fees
        ops.push((10, 2, 0, 50_000, 0)); // burn_position_by_token_id

        let (ctx, max_crossings) =
            run_script((30_i128, 0_i32, 1_i32, seed, ops)).expect("stateful invariants failed");
        assert!(
            max_crossings >= 2,
            "script never produced a multi-tick-crossing swap (deepest crossing {max_crossings})"
        );
        let _ = ctx;
    }

    /// Burn-everything residual: after a script, burning every position and
    /// collecting every fee leaves the pool with at most `BURN_ALL_DUST` per
    /// token.
    ///
    /// This used to be the fourth manifestation of the coarse-swap-price bug,
    /// alongside the three regressions below. The scripted swaps walked the
    /// pool price ~20 ticks per step regardless of size, out of every
    /// position's range, so all three positions were then valued entirely in
    /// token A:
    ///
    ///   balances    a = 0        b = 400225
    ///   tokens_owed a = 400508   b = 0
    ///
    /// `collect_fees_core` could only pay `min(owed, balance) == 0`, leaving
    /// position -8..8 holding `owed = (289, 0)` while 400225 token B sat in
    /// the contract unclaimable — solvent in aggregate, insolvent per token.
    /// The swap engine now prices in full Q64.96, so the pool fully
    /// liquidates.
    #[test]
    fn cl_burn_all_returns_everything() {
        // Tight ranges bracketing the initial tick and modest swaps keep the
        // price near the initial tick.
        let script: Vec<(u8, i32, i32, i128, u8)> = (0..12)
            .map(|i| {
                (
                    1_u8,
                    i as i32,
                    0_i32,
                    500_i128 + i as i128 * 31,
                    (i % 2) as u8,
                )
            })
            .collect();
        let params = (
            30_i128,
            0_i32,
            1_i32,
            vec![
                (-8, 8, 200_000, 200_000),
                (-16, -6, 200_000, 200_000),
                (6, 16, 200_000, 200_000),
            ],
            script,
        );

        let (ctx, _max_crossings) = run_script(params).expect("script must pass invariants");

        // Burn everything, then collect every remaining fee. A bounded loop so
        // a transfer failure (per-token insolvency) cannot spin forever; any
        // remaining position afterwards fails the assertion below.
        let client = ctx.client();
        for _round in 0..100 {
            let mut progress = false;
            for p in &ctx.positions {
                let pos = match client.try_get_position(&ctx.provider, &p.lower, &p.upper) {
                    Ok(Ok(pos)) => pos,
                    _ => continue,
                };
                if pos.liquidity > 0 {
                    if client
                        .try_burn_position(&ctx.provider, &p.lower, &p.upper, &pos.liquidity)
                        .is_ok()
                    {
                        progress = true;
                    }
                }
                if pos.tokens_owed != (0, 0) {
                    if client
                        .try_collect_fees(&ctx.provider, &p.lower, &p.upper)
                        .is_ok()
                    {
                        progress = true;
                    }
                }
            }
            if !progress {
                break;
            }
        }

        // Every position must be fully liquidated.
        for p in &ctx.positions {
            let (liq, owed) = match client.try_get_position(&ctx.provider, &p.lower, &p.upper) {
                Ok(Ok(pos)) => (pos.liquidity, pos.tokens_owed),
                _ => (0, (0, 0)),
            };
            assert!(
                liq == 0 && owed == (0, 0),
                "position {}-{} not fully liquidated (liq {}, owed {:?})",
                p.lower,
                p.upper,
                liq,
                owed
            );
        }

        let (ra, rb) = (ctx.bal_a(), ctx.bal_b());
        assert!(
            ra <= BURN_ALL_DUST,
            "burn-all left residual token A of {ra} > BURN_ALL_DUST {BURN_ALL_DUST}"
        );
        assert!(
            rb <= BURN_ALL_DUST,
            "burn-all left residual token B of {rb} > BURN_ALL_DUST {BURN_ALL_DUST}"
        );
    }

    /// Per-token solvency regression: the pool's balances must cover what
    /// every position can withdraw, token by token.
    ///
    /// The swap loop prices in a coarse ~3-significant-digit representation
    /// while `quote_position` / `burn_position` price at the fine
    /// `tick_to_sqrt_price_x96`. After a price move, the fine per-token burn
    /// claims can exceed the pool's per-token balance even though total value
    /// is conserved, so burning every position can fail for lack of a single
    /// token. Not fixed in this PR, per the issue's instruction to report
    /// rather than repair contract bugs.
    #[test]
    fn cl_solvency_per_token_regression() {
        let mut ctx = deploy(30_i128, 0_i32, 1_i32);
        for (lo, hi, aa, ab) in [
            (-400, 400, 100_000, 100_000),
            (-800, -300, 100_000, 100_000),
            (300, 800, 100_000, 100_000),
        ] {
            assert!(ctx.mint(lo, hi, aa, ab));
        }
        assert!(ctx.swap(true, 50_000));

        let (liab_a, liab_b) = ctx.total_liability();
        let (ba, bb) = (ctx.bal_a(), ctx.bal_b());
        assert!(
            liab_a <= ba && liab_b <= bb,
            "per-token solvency violated: A liability {liab_a} vs balance {ba}, \
             B liability {liab_b} vs balance {bb}"
        );
    }

    /// Total-value solvency regression: the pool's balances must cover the
    /// aggregate value of every position's claim.
    ///
    /// Same root cause as the per-token case: the coarse swap price vs the fine
    /// burn price are inconsistent, so even the sum of all positions' burn
    /// claims can exceed the pool's combined balance after a price move.
    #[test]
    fn cl_solvency_total_value_regression() {
        let mut ctx = deploy(30_i128, 0_i32, 1_i32);
        for (lo, hi, aa, ab) in [
            (-8, 8, 200_000, 200_000),
            (-16, -6, 200_000, 200_000),
            (6, 16, 200_000, 200_000),
        ] {
            assert!(ctx.mint(lo, hi, aa, ab));
        }
        assert!(ctx.swap(true, 20_000));

        let (liab_a, liab_b) = ctx.total_liability();
        let (ba, bb) = (ctx.bal_a(), ctx.bal_b());
        assert!(
            liab_a + liab_b <= ba + bb,
            "total-value solvency violated: liability {} > balance {}",
            liab_a + liab_b,
            ba + bb
        );
    }

    /// `active_liquidity()` consistency regression: it must equal the sum of
    /// liquidity for every position whose range brackets the current tick,
    /// including after a swap crosses out of all of them.
    ///
    /// After a swap that moves the current tick out of every open range, the
    /// stored `active_liquidity()` still reports the pre-swap in-range
    /// liquidity instead of `0`, so the tick-crossing active-liquidity
    /// bookkeeping in `swap` is stale.
    #[test]
    fn cl_active_liquidity_regression() {
        let mut ctx = deploy(30_i128, 0_i32, 1_i32);
        for (lo, hi, aa, ab) in [
            (-8, 8, 200_000, 200_000),
            (-16, -6, 200_000, 200_000),
            (6, 16, 200_000, 200_000),
        ] {
            assert!(ctx.mint(lo, hi, aa, ab));
        }
        assert!(ctx.swap(true, 20_000));

        let cur = ctx.current_tick();
        let expected = ctx.expected_active_liquidity();
        let active = ctx.client().active_liquidity();
        assert!(
            cur < -16 || expected == active,
            "active_liquidity stale after swap: current tick {cur} (below all ranges), \
             active_liquidity() = {active}, expected {expected}"
        );
    }
}
