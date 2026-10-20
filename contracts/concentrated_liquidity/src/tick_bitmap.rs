//! Packed tick bitmap for O(1) next-initialized-tick lookup.
//!
//! Each word covers 128 ticks (u128 = 128 bits).
//! word_pos = tick >> 7   (i32 key)
//! bit_pos  = tick & 0x7F (0..127)
//!
//! Storage key: DataKey::TickBitmap(word_pos: i32)

#![allow(dead_code)]

use soroban_sdk::Env;

use crate::DataKey;

fn get_word(env: &Env, key: &DataKey) -> u128 {
    if let Some(w) = env.storage().persistent().get(key) {
        env.storage().persistent().extend_ttl(
            key,
            crate::POSITION_TTL_THRESHOLD,
            crate::POSITION_BUMP_TO,
        );
        w
    } else {
        env.storage().instance().get(key).unwrap_or(0)
    }
}

/// Flip the bit for `tick` in its bitmap word.
/// Called on first reference (mint) and when liquidity_gross reaches zero (burn).
pub fn flip_tick(env: &Env, tick: i32) {
    let (word_pos, bit_pos) = tick_position(tick);
    let key = DataKey::TickBitmap(word_pos);
    let word = get_word(env, &key);
    let toggled = word ^ (1u128 << bit_pos);
    if toggled == 0 {
        env.storage().persistent().remove(&key);
        env.storage().instance().remove(&key);
    } else {
        env.storage().persistent().set(&key, &toggled);
        env.storage().persistent().extend_ttl(
            &key,
            crate::POSITION_TTL_THRESHOLD,
            crate::POSITION_BUMP_TO,
        );
        env.storage().instance().remove(&key);
    }
}

/// Find the next initialized tick within the same word as `tick`.
///
/// `lte = true`  → search for the next tick ≤ current (right-to-left within word).
/// `lte = false` → search for the next tick ≥ current (left-to-right within word).
///
/// Returns `(tick_index, found)`. If no initialized tick exists in this word
/// in the requested direction, `found` is false and `tick_index` is the
/// boundary of the word.
pub fn next_initialized_tick_within_word(env: &Env, tick: i32, lte: bool) -> (i32, bool) {
    let (word_pos, bit_pos) = tick_position(tick);
    let key = DataKey::TickBitmap(word_pos);
    let word = get_word(env, &key);

    if lte {
        // Mask out all bits above bit_pos, then find the highest set bit.
        let mask = if bit_pos == 127 {
            u128::MAX
        } else {
            (1u128 << (bit_pos + 1)) - 1
        };
        let masked = word & mask;
        if masked == 0 {
            // No initialized tick at or below `tick` in this word.
            let word_base = word_pos * 128;
            return (word_base, false);
        }
        let next_bit = 127 - masked.leading_zeros() as i32;
        let tick_index = word_pos * 128 + next_bit;
        (tick_index, true)
    } else {
        // Mask out all bits below bit_pos, then find the lowest set bit.
        let mask = if bit_pos == 0 {
            u128::MAX
        } else {
            u128::MAX.wrapping_shl(bit_pos)
        };
        let masked = word & mask;
        if masked == 0 {
            // No initialized tick at or above `tick` in this word.
            let word_base = word_pos * 128 + 127;
            return (word_base, false);
        }
        let next_bit = masked.trailing_zeros() as i32;
        let tick_index = word_pos * 128 + next_bit;
        (tick_index, true)
    }
}

/// Decompose a tick into (word_pos, bit_pos).
/// word_pos = tick >> 7   (divides by 128)
/// bit_pos  = tick & 0x7F (remainder mod 128, but ticks can be negative)
///
/// For negative ticks Rust's `>>` is arithmetic (sign-extending), giving the
/// correct floor division. `& 0x7F` extracts the low 7 bits giving 0..=127.
fn tick_position(tick: i32) -> (i32, u32) {
    // Rust arithmetic right-shift gives floor division for negative numbers.
    let word_pos = tick >> 7;
    // bit_pos is always positive (0..127).
    let bit_pos = (tick & 0x7F) as u32;
    (word_pos, bit_pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConcentratedLiquidity;
    use soroban_sdk::Env;

    fn env_with_contract() -> (Env, soroban_sdk::Address) {
        let env = Env::default();
        env.mock_all_auths();
        let addr = env.register_contract(None, ConcentratedLiquidity);
        (env, addr)
    }

    #[test]
    fn flip_sets_and_clears_bit() {
        let (env, addr) = env_with_contract();
        let tick = 256i32; // word 2, bit 0
        env.as_contract(&addr, || {
            // Initially unset
            let (_, found_before) = next_initialized_tick_within_word(&env, tick, false);
            assert!(!found_before);

            flip_tick(&env, tick);
            let (t, found) = next_initialized_tick_within_word(&env, tick, false);
            assert!(found);
            assert_eq!(t, tick);

            // Flip again to clear
            flip_tick(&env, tick);
            let (_, found_after) = next_initialized_tick_within_word(&env, tick, false);
            assert!(!found_after);
        });
    }

    #[test]
    fn next_tick_lte_finds_correct_tick() {
        let (env, addr) = env_with_contract();
        env.as_contract(&addr, || {
            // Set tick 10 and tick 5 in word 0
            flip_tick(&env, 10);
            flip_tick(&env, 5);

            // From tick 8 searching downward, should find tick 5
            let (t, found) = next_initialized_tick_within_word(&env, 8, true);
            assert!(found);
            assert_eq!(t, 5);
        });
    }

    #[test]
    fn next_tick_gte_finds_correct_tick() {
        let (env, addr) = env_with_contract();
        env.as_contract(&addr, || {
            flip_tick(&env, 10);
            flip_tick(&env, 20);

            // From tick 12, searching upward, should find 20
            let (t, found) = next_initialized_tick_within_word(&env, 12, false);
            assert!(found);
            assert_eq!(t, 20);
        });
    }

    #[test]
    fn multiple_ticks_same_word() {
        let (env, addr) = env_with_contract();
        env.as_contract(&addr, || {
            flip_tick(&env, 0);
            flip_tick(&env, 64);
            flip_tick(&env, 127);

            let (t, found) = next_initialized_tick_within_word(&env, 0, false);
            assert!(found);
            assert_eq!(t, 0);

            let (t, found) = next_initialized_tick_within_word(&env, 1, false);
            assert!(found);
            assert_eq!(t, 64);

            let (t, found) = next_initialized_tick_within_word(&env, 127, true);
            assert!(found);
            assert_eq!(t, 127);
        });
    }

    #[test]
    fn no_tick_in_direction_returns_false() {
        let (env, addr) = env_with_contract();
        env.as_contract(&addr, || {
            flip_tick(&env, 100);

            let (_, found) = next_initialized_tick_within_word(&env, 99, true);
            assert!(!found);
        });
    }
}
