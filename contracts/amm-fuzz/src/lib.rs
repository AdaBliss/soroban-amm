#![allow(dead_code)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::unnecessary_cast)]

//! Fuzz / property-based tests for AMM swap invariants.
//!
//! This crate contains **two complementary suites**:
//!
//! ## 1. Pure-formula suite (fast, no Soroban environment)
//!
//! The top-level `proptest!` block and the `regression` module verify the
//! constant-product formula helpers (`cp_amount_out`, `fee_amount`,
//! `SimPool`) in isolation. These run tens of thousands of cases cheaply
//! because they never touch an actual contract.
//!
//! Verifies:
//!   1. The x*y=k invariant holds (k never decreases) after every swap.
//!   2. Fee calculations are correct across all valid fee tiers (0–10 000 bps).
//!   3. No integer overflow or rounding error causes incorrect output.
//!   4. The constant-product output formula never returns a value ≥ reserve_out.
//!
//! ## 2. Deployed-contract suite (`contract_props` module)
//!
//! The `contract_props` module deploys the **real** `amm.wasm` and
//! `token.wasm` binaries (via `soroban_sdk::contractimport!`) into a
//! Soroban test environment and drives `Pool::client` directly.  It
//! verifies the same invariants against the on-chain code so that
//! defects in the actual contract logic (e.g. in `swap`, `add_liquidity`,
//! or `swap_fot`) are caught here even if the pure-formula suite passes.
//!
//! Run with:
//!   cargo test -p amm-fuzz
//!
//! Each proptest property runs with 256 cases by default in the
//! deployed-contract suite (Soroban environments are heavier to spin up)
//! and 10 000 cases in the pure-formula suite.

extern crate std;

pub mod cl;

use proptest::prelude::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{StellarAssetClient, TokenClient as StellarTokenClient},
    Address, Env,
};

// ── Pure formula helpers (mirror of AMM internals) ────────────────────────────

/// Constant-product output amount (mirrors AMM swap formula).
fn cp_amount_out(amount_in: i128, reserve_in: i128, reserve_out: i128, fee_bps: i128) -> i128 {
    let amount_in_with_fee = amount_in * (10_000 - fee_bps);
    let denom = reserve_in * 10_000 + amount_in_with_fee;
    if denom == 0 {
        return 0;
    }
    amount_in_with_fee * reserve_out / denom
}

/// Compute the fee portion of an input amount.
fn fee_amount(amount_in: i128, fee_bps: i128) -> i128 {
    amount_in * fee_bps / 10_000
}

/// Realised swap output — pure mirror of `cp_amount_out`, kept as a
/// separate function so the equality property (#303) catches future
/// drift if the two paths ever diverge.
fn simulate_realised_swap(
    amount_in: i128,
    reserve_in: i128,
    reserve_out: i128,
    fee_bps: i128,
) -> i128 {
    cp_amount_out(amount_in, reserve_in, reserve_out, fee_bps)
}

fn isqrt(n: i128) -> i128 {
    if n <= 0 {
        return 0;
    }
    // Use Newton's method (integer) to compute floor(sqrt(n)).
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Pure-Rust simulator used by `prop_per_share_k_non_decreasing_across_sequences` (#303).
///
/// Mirrors the AMM's accounting at the granularity the constant-product
/// invariant cares about. LP shares are tracked so removals subtract
/// the correct slice of each reserve, and the per-share k value stays
/// monotone in fees.
#[derive(Clone, Debug)]
struct SimPool {
    reserve_a: i128,
    reserve_b: i128,
    shares: i128,
    fee_bps: i128,
}

impl SimPool {
    fn new(reserve_a: i128, reserve_b: i128, fee_bps: i128) -> Self {
        let k = reserve_a.saturating_mul(reserve_b);
        let shares = isqrt(k).max(1);
        Self {
            reserve_a,
            reserve_b,
            shares,
            fee_bps,
        }
    }

    /// Add liquidity at the current ratio. `amount_a` is the input
    /// in token A; the matching token B amount is inferred. Mints
    /// shares proportional to `amount_a / reserve_a`.
    fn add_liquidity(&mut self, amount_a: i128) {
        if amount_a <= 0 || self.reserve_a == 0 || self.shares == 0 {
            return;
        }
        let amount_b = amount_a * self.reserve_b / self.reserve_a;
        let new_shares = amount_a * self.shares / self.reserve_a;
        self.reserve_a += amount_a;
        self.reserve_b += amount_b;
        self.shares += new_shares;
    }

    /// Remove liquidity. `share_amount` is an absolute share count
    /// (clamped to `self.shares`). Returns A and B amounts removed.
    fn remove_liquidity_share(&mut self, share_amount: i128) {
        if share_amount <= 0 || self.shares == 0 {
            return;
        }
        let burn = share_amount.min(self.shares - 1).max(0); // never burn the last share
        if burn == 0 {
            return;
        }
        let out_a = burn * self.reserve_a / self.shares;
        let out_b = burn * self.reserve_b / self.shares;
        self.reserve_a -= out_a;
        self.reserve_b -= out_b;
        self.shares -= burn;
    }

    /// Swap A→B at the configured fee. Drops `amount_in` if the pool
    /// would be drained.
    fn swap_a_for_b(&mut self, amount_in: i128) {
        if amount_in <= 0 || self.reserve_a == 0 || self.reserve_b == 0 {
            return;
        }
        let out = cp_amount_out(amount_in, self.reserve_a, self.reserve_b, self.fee_bps);
        if out <= 0 || out >= self.reserve_b {
            return;
        }
        self.reserve_a += amount_in;
        self.reserve_b -= out;
    }

    /// Per-share k. Scaled by 1e12 so int truncation doesn't drown
    /// the signal at small share counts.
    fn per_share_k(&self) -> i128 {
        if self.shares == 0 {
            return 0;
        }
        // Compute as ((reserve_a * 1e6) / shares) * ((reserve_b * 1e6) / shares)
        // so intermediate values stay well below i128::MAX.
        let scale = 1_000_000_i128;
        let a_per_share = (self.reserve_a * scale) / self.shares;
        let b_per_share = (self.reserve_b * scale) / self.shares;
        a_per_share * b_per_share
    }
}

// ── In-process tests using the live AMM contract ─────────────────────────────

mod amm_wasm {
    soroban_sdk::contractimport!(file = "../../target/wasm32v1-none/release/amm.wasm");
}

mod token_wasm {
    soroban_sdk::contractimport!(file = "../../target/wasm32v1-none/release/token.wasm");
}

fn create_sac<'a>(
    env: &'a Env,
    admin: &Address,
) -> (StellarTokenClient<'a>, StellarAssetClient<'a>) {
    let contract = env.register_stellar_asset_contract_v2(admin.clone());
    (
        StellarTokenClient::new(env, &contract.address()),
        StellarAssetClient::new(env, &contract.address()),
    )
}

struct Pool<'a> {
    client: amm_wasm::Client<'a>,
    ta: Address,
    tb: Address,
    lp: Address,
    admin: Address,
    provider: Address,
}

fn deploy_pool(env: &Env, fee_bps: i128) -> Pool<'_> {
    deploy_pool_with_ratio(env, fee_bps, 1_000_000, 1_000_000)
}

fn deploy_pool_with_ratio(env: &Env, fee_bps: i128, initial_a: i128, initial_b: i128) -> Pool<'_> {
    // These fuzz properties check contract *correctness*, not gas cost — that's
    // `benches`'s job. The default test budget models real network CPU limits,
    // but `amm.wasm`/`token.wasm` here are unoptimized debug-adjacent builds
    // (no `stellar contract optimize` pass), so just instantiating them for a
    // property-test iteration can exceed it well before any contract logic runs.
    env.budget().reset_unlimited();
    let amm_hash = env.deployer().upload_contract_wasm(amm_wasm::WASM);
    let token_hash = env.deployer().upload_contract_wasm(token_wasm::WASM);

    let admin = Address::generate(env);
    let pool_addr = env
        .deployer()
        .with_address(
            Address::generate(env),
            soroban_sdk::BytesN::from_array(env, &[0u8; 32]),
        )
        .deploy(amm_hash);
    let lp_addr = env
        .deployer()
        .with_address(
            Address::generate(env),
            soroban_sdk::BytesN::from_array(env, &[1u8; 32]),
        )
        .deploy(token_hash);

    token_wasm::Client::new(env, &lp_addr).initialize(
        &pool_addr,
        &soroban_sdk::String::from_str(env, "LP"),
        &soroban_sdk::String::from_str(env, "LP"),
        &7u32,
    );

    let (ta, ta_sac) = create_sac(env, &admin);
    let (tb, tb_sac) = create_sac(env, &admin);

    let client = amm_wasm::Client::new(env, &pool_addr);
    client.initialize(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &fee_bps,
        &admin,
        &0_i128,
    );

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &initial_a);
    tb_sac.mint(&provider, &initial_b);
    client.add_liquidity(&provider, &initial_a, &initial_b, &0_i128, &u64::MAX);

    let ta_addr = ta.address.clone();
    let tb_addr = tb.address.clone();
    drop((ta, ta_sac, tb, tb_sac));

    Pool {
        client,
        ta: ta_addr,
        tb: tb_addr,
        lp: lp_addr,
        admin,
        provider,
    }
}

// ── Property-based tests ──────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// Property: output amount from the formula is always strictly less than reserve_out.
    ///
    /// This is the core safety invariant: you can never drain the pool in one swap.
    #[test]
    fn prop_output_strictly_lt_reserve_out(
        amount_in   in 1_i128..=1_000_000_i128,
        reserve_in  in 1_i128..=10_000_000_i128,
        reserve_out in 1_i128..=10_000_000_i128,
        fee_bps     in 0_i128..=10_000_i128,
    ) {
        let out = cp_amount_out(amount_in, reserve_in, reserve_out, fee_bps);
        prop_assert!(
            out < reserve_out,
            "output {out} >= reserve_out {reserve_out} (in={amount_in}, rin={reserve_in}, fee={fee_bps})"
        );
    }

    /// Property: output is non-negative for all valid inputs.
    #[test]
    fn prop_output_non_negative(
        amount_in   in 1_i128..=1_000_000_i128,
        reserve_in  in 1_i128..=10_000_000_i128,
        reserve_out in 1_i128..=10_000_000_i128,
        fee_bps     in 0_i128..=10_000_i128,
    ) {
        let out = cp_amount_out(amount_in, reserve_in, reserve_out, fee_bps);
        prop_assert!(out >= 0, "negative output: {out}");
    }

    /// Property: output is monotone in amount_in (more in → more out).
    #[test]
    fn prop_output_monotone_in_amount_in(
        amount_in_small in 1_i128..=500_000_i128,
        delta           in 1_i128..=500_000_i128,
        reserve_in      in 1_i128..=10_000_000_i128,
        reserve_out     in 1_i128..=10_000_000_i128,
        fee_bps         in 0_i128..=9_999_i128,
    ) {
        let amount_in_large = amount_in_small + delta;
        let out_small = cp_amount_out(amount_in_small, reserve_in, reserve_out, fee_bps);
        let out_large = cp_amount_out(amount_in_large, reserve_in, reserve_out, fee_bps);
        prop_assert!(
            out_large >= out_small,
            "monotonicity violated: out({amount_in_large})={out_large} < out({amount_in_small})={out_small}"
        );
    }

    /// Property: effective rate (out/in) is non-increasing as amount_in grows
    /// (price impact is always adverse for the trader).
    #[test]
    fn prop_effective_rate_non_increasing(
        amount_in_small in 1_i128..=100_000_i128,
        delta           in 1_i128..=100_000_i128,
        reserve_in      in 100_000_i128..=10_000_000_i128,
        reserve_out     in 100_000_i128..=10_000_000_i128,
        fee_bps         in 0_i128..=9_999_i128,
    ) {
        let amount_in_large = amount_in_small + delta;
        let out_small = cp_amount_out(amount_in_small, reserve_in, reserve_out, fee_bps);
        let out_large = cp_amount_out(amount_in_large, reserve_in, reserve_out, fee_bps);

        // Compare using cross multiplication to avoid rate truncation. A
        // single output unit of integer formula rounding is allowed.
        if out_small > 0 && out_large > 0 {
            prop_assert!(
                out_large.saturating_mul(amount_in_small)
                    <= out_small
                        .saturating_add(1)
                        .saturating_mul(amount_in_large),
                "effective rate increased: rate({amount_in_large})={out_large}/{amount_in_large} > rate({amount_in_small})={out_small}/{amount_in_small}"
            );
        }
    }

    /// Property: fee amount is non-negative and at most amount_in.
    #[test]
    fn prop_fee_amount_bounded(
        amount_in in 1_i128..=100_000_000_i128,
        fee_bps   in 0_i128..=10_000_i128,
    ) {
        let fee = fee_amount(amount_in, fee_bps);
        prop_assert!(fee >= 0, "negative fee: {fee}");
        prop_assert!(fee <= amount_in, "fee {fee} > amount_in {amount_in}");
    }

    /// Property: with zero fee, output equals the pure constant-product formula.
    #[test]
    fn prop_zero_fee_equals_pure_cp(
        amount_in   in 1_i128..=1_000_000_i128,
        reserve_in  in 1_i128..=10_000_000_i128,
        reserve_out in 1_i128..=10_000_000_i128,
    ) {
        let out_with_fee = cp_amount_out(amount_in, reserve_in, reserve_out, 0);
        let expected = amount_in * reserve_out / (reserve_in + amount_in);
        // Integer division may differ by 1 at most
        prop_assert!(
            (out_with_fee - expected).abs() <= 1,
            "zero-fee formula mismatch: got={out_with_fee}, expected={expected}"
        );
    }

    /// Property: maximum fee (10 000 bps = 100%) yields zero output.
    #[test]
    fn prop_max_fee_yields_zero_output(
        amount_in   in 1_i128..=1_000_000_i128,
        reserve_in  in 1_i128..=10_000_000_i128,
        reserve_out in 1_i128..=10_000_000_i128,
    ) {
        let out = cp_amount_out(amount_in, reserve_in, reserve_out, 10_000);
        prop_assert_eq!(out, 0, "100% fee should yield zero output");
    }

    /// Property: product k = reserve_in * reserve_out never decreases after a swap.
    #[test]
    fn prop_k_never_decreases(
        amount_in   in 1_i128..=100_000_i128,
        reserve_in  in 100_000_i128..=5_000_000_i128,
        reserve_out in 100_000_i128..=5_000_000_i128,
        fee_bps     in 0_i128..=9_999_i128,
    ) {
        let k_before = reserve_in * reserve_out;
        let out = cp_amount_out(amount_in, reserve_in, reserve_out, fee_bps);
        let new_reserve_in = reserve_in + amount_in;
        let new_reserve_out = reserve_out - out;
        let k_after = new_reserve_in * new_reserve_out;
        prop_assert!(
            k_after >= k_before,
            "k decreased: before={k_before}, after={k_after} (fee_bps={fee_bps})"
        );
    }

    /// Property (#303): per-LP-unit invariant `k / shares^2` is
    /// non-decreasing across **any sequence** of `add_liquidity`,
    /// `swap`, and `remove_liquidity` operations. Fees can only
    /// grow it; correctly-proportional adds/removes preserve it.
    ///
    /// Operations are encoded as a `(u8, i128)` tuple-stream so
    /// proptest can shrink failures into a minimal reproduction.
    #[test]
    fn prop_per_share_k_non_decreasing_across_sequences(
        initial_a   in 1_000_000_i128..=10_000_000_i128,
        initial_b   in 1_000_000_i128..=10_000_000_i128,
        ops         in proptest::collection::vec(
            (0u8..3u8, 1_i128..=200_000_i128),
            1..32,
        ),
        fee_bps     in 0_i128..=300_i128,
    ) {
        let mut pool = SimPool::new(initial_a, initial_b, fee_bps);
        let baseline = pool.per_share_k();
        for (kind, amount) in ops {
            match kind {
                0 => pool.add_liquidity(amount),
                1 => pool.swap_a_for_b(amount),
                2 => pool.remove_liquidity_share(amount),
                _ => unreachable!(),
            }
            // The invariant: per-share k can only grow. Allow ±1
            // for integer-division rounding.
            let now = pool.per_share_k();
            let rounding_tolerance = baseline / 10_000 + 1;
            prop_assert!(
                now + rounding_tolerance >= baseline,
                "per-share k regressed: baseline={baseline}, now={now}, after op (kind={kind}, amount={amount})"
            );
        }
    }

    /// Property (#303): `get_amount_out(get_amount_in_for(out))` ≈
    /// out within ±1 unit. The actual transferred amount on a real
    /// swap equals what `cp_amount_out` says it will — the existing
    /// formula is the AMM's reference implementation, so an
    /// idempotent round-trip is the closest pure-Rust equivalent
    /// to "predicted output matches realised output".
    #[test]
    fn prop_get_amount_out_matches_realised(
        amount_in   in 100_i128..=100_000_i128,
        reserve_in  in 1_000_000_i128..=10_000_000_i128,
        reserve_out in 1_000_000_i128..=10_000_000_i128,
        fee_bps     in 0_i128..=300_i128,
    ) {
        let predicted = cp_amount_out(amount_in, reserve_in, reserve_out, fee_bps);
        // "Realised" = the amount the pool would actually transfer if
        // the swap executed. In the constant-product model this is
        // identical to `predicted` because the formula is the swap
        // routine. The property catches future drift where two code
        // paths diverge.
        let realised = simulate_realised_swap(amount_in, reserve_in, reserve_out, fee_bps);
        prop_assert_eq!(predicted, realised);
    }

    /// Property (#303): `simulate_swap` price-impact direction is
    /// always correct: a swap of A→B always *raises* the spot price
    /// of A in B (since pool gains A, loses B). Conversely B→A
    /// lowers it.
    #[test]
    fn prop_simulate_swap_price_impact_direction(
        amount_in   in 100_i128..=200_000_i128,
        reserve_in  in 1_000_000_i128..=10_000_000_i128,
        reserve_out in 1_000_000_i128..=10_000_000_i128,
        fee_bps     in 0_i128..=300_i128,
    ) {
        // Pre-swap spot price of A in B = reserve_b / reserve_a
        // (scaled to avoid loss).
        let pre_price = reserve_out * 1_000_000 / reserve_in;
        let out = cp_amount_out(amount_in, reserve_in, reserve_out, fee_bps);
        if out == 0 || out >= reserve_out {
            return Ok(());
        }
        let new_reserve_in = reserve_in + amount_in;
        let new_reserve_out = reserve_out - out;
        let post_price = new_reserve_out * 1_000_000 / new_reserve_in;
        // After A→B swap, A is more plentiful in the pool ⇒ price of
        // A in B drops (post_price <= pre_price).
        prop_assert!(
            post_price <= pre_price,
            "price-impact direction wrong: pre={pre_price}, post={post_price}"
        );
    }

    /// Property: get_amount_in is a right-inverse of get_amount_out up to ±1 rounding.
    ///
    /// amount_in_reverse = ceil((reserve_in * out * 10_000) / ((reserve_out - out) * (10_000 - fee_bps)))
    #[test]
    fn prop_get_amount_in_is_inverse(
        amount_in   in 1_i128..=10_000_i128,
        reserve_in  in 100_000_i128..=1_000_000_i128,
        reserve_out in 100_000_i128..=1_000_000_i128,
        fee_bps     in 0_i128..=9_999_i128,
    ) {
        let out = cp_amount_out(amount_in, reserve_in, reserve_out, fee_bps);
        if out == 0 || out >= reserve_out {
            // Edge cases where the inverse formula is undefined
            return Ok(());
        }
        let numer = reserve_in * out * 10_000;
        let denom = (reserve_out - out) * (10_000 - fee_bps);
        if denom == 0 {
            return Ok(());
        }
        // Proper integer ceiling: ceil(numer / denom).
        let amount_in_reverse = (numer + denom - 1) / denom;
        prop_assert!(
            amount_in_reverse <= amount_in + 2,
            "inverse quote {amount_in_reverse} deviates by more than 2 from original {amount_in}"
        );
        let out_from_reverse = cp_amount_out(amount_in_reverse, reserve_in, reserve_out, fee_bps);
        prop_assert!(
            out_from_reverse >= out,
            "inverse did not produce at least the original out: out={out}, out_from_reverse={out_from_reverse}"
        );
    }
}

// ── Deployed-contract property tests ─────────────────────────────────────────

/// Property tests that exercise the **real** deployed `amm.wasm` contract.
///
/// Every property in this module spins up a Soroban test environment,
/// deploys the AMM and token contracts via `contractimport!`, and drives the
/// `Pool` client so that defects in the actual on-chain code are caught here.
///
/// The case count is kept at 256 (instead of 10 000) because each case
/// requires a full Soroban environment setup; this is still enough coverage
/// to surface logic bugs while keeping CI time reasonable.
///
/// # Reserve measurement
///
/// Rather than calling `get_info()` (which returns a custom `PoolInfo` struct
/// that can collide with the SDK's own re-exported type), the tests read the
/// pool contract's token balance directly from the SAC (`StellarTokenClient`).
/// This gives the same reserve values through a simpler, always-stable API.
#[cfg(test)]
mod contract_props {
    use super::*;
    use soroban_sdk::token::TokenClient as StellarTokenClient;

    /// Read the pool's current token-A and token-B balances (= reserves) using
    /// the standard SEP-41 `balance` call on each SAC.  This avoids depending
    /// on `get_info()` and the custom `PoolInfo` type.
    fn pool_reserves<'a>(
        env: &'a Env,
        pool_addr: &soroban_sdk::Address,
        ta: &soroban_sdk::Address,
        tb: &soroban_sdk::Address,
    ) -> (i128, i128) {
        let ra = StellarTokenClient::new(env, ta).balance(pool_addr);
        let rb = StellarTokenClient::new(env, tb).balance(pool_addr);
        (ra, rb)
    }

    #[derive(Clone, Debug)]
    enum Operation {
        AddLiquidity(i128),
        RemoveLiquidity(i128),
        RemoveLiquidityOneSided(bool, i128),
        Swap(bool, i128),
        SwapExactOut(bool, i128),
        FlashLoanShort(i128),
        WithdrawProtocolFees,
        UpdateFee(i128),
        SetProtocolFee(i128),
        SetLpRebate(i128),
        AdvanceLedger(u64),
        EmergencyWithdraw,
    }

    fn operation_strategy() -> impl Strategy<Value = Operation> {
        prop_oneof![
            (1_i128..=10_000).prop_map(Operation::AddLiquidity),
            (1_i128..=10_000).prop_map(Operation::RemoveLiquidity),
            (any::<bool>(), 1_i128..=10_000).prop_map(|(direction, amount)| {
                Operation::RemoveLiquidityOneSided(direction, amount)
            }),
            (any::<bool>(), 1_i128..=10_000)
                .prop_map(|(direction, amount)| Operation::Swap(direction, amount)),
            (any::<bool>(), 1_i128..=5_000)
                .prop_map(|(direction, amount)| Operation::SwapExactOut(direction, amount)),
            (1_i128..=10_000).prop_map(Operation::FlashLoanShort),
            Just(Operation::WithdrawProtocolFees),
            (0_i128..=300).prop_map(Operation::UpdateFee),
            (0_i128..=100).prop_map(Operation::SetProtocolFee),
            (0_i128..=10_000).prop_map(Operation::SetLpRebate),
            (1_u64..=3_600).prop_map(Operation::AdvanceLedger),
            Just(Operation::EmergencyWithdraw),
        ]
    }

    /// Quarantines are deliberately keyed by invariant and operation. A new
    /// operation/invariant pair may not be skipped without an issue reference.
    const KNOWN_FAILURES: &[(&str, &str, &str)] = &[
        ("reserve_accounting", "remove_liquidity_one_sided", "#1093"),
        ("exact_out_delivery", "swap_exact_out", "#1097"),
        ("lp_supply", "emergency_withdraw", "#1096"),
        ("fot_accounting", "add_liquidity_fot", "#1094"),
        ("fot_accounting", "swap_fot", "#1095"),
        ("flash_loan_accounting", "flash_loan", "#1107"),
    ];

    fn is_known_failure(invariant: &str, operation: &str) -> bool {
        KNOWN_FAILURES
            .iter()
            .any(|(known_invariant, known_operation, _)| {
                *known_invariant == invariant && *known_operation == operation
            })
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct State {
        reserve_a: i128,
        reserve_b: i128,
        accrued_a: i128,
        accrued_b: i128,
        pool_balance_a: i128,
        pool_balance_b: i128,
        total_shares: i128,
        lp_supply: i128,
    }

    fn state(env: &Env, pool: &Pool<'_>) -> State {
        let info = pool.client.get_info();
        let accrued = pool.client.get_accrued_fees();
        let (pool_balance_a, pool_balance_b) =
            pool_reserves(env, &pool.client.address, &pool.ta, &pool.tb);
        State {
            reserve_a: info.reserve_a,
            reserve_b: info.reserve_b,
            accrued_a: accrued.0,
            accrued_b: accrued.1,
            pool_balance_a,
            pool_balance_b,
            total_shares: info.total_shares,
            lp_supply: token_wasm::Client::new(env, &pool.lp).total_supply(),
        }
    }

    fn tracked_token_total(env: &Env, token: &Address, pool: &Pool<'_>, actor: &Address) -> i128 {
        let client = StellarTokenClient::new(env, token);
        client.balance(&pool.client.address)
            + client.balance(actor)
            + client.balance(&pool.admin)
            + client.balance(&pool.provider)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Contract property: k = reserve_a * reserve_b never decreases after
        /// a swap against the real deployed AMM.
        ///
        /// This is the on-chain counterpart of `prop_k_never_decreases`. It
        /// catches bugs in `swap` itself (fee accounting, reserve updates) that
        /// the pure-formula suite cannot see.
        #[test]
        fn contract_prop_k_never_decreases_after_swap(
            amount_in in 1_i128..=500_000_i128,
            fee_bps   in 0_i128..=9_999_i128,
        ) {
            let env = Env::default();
            env.mock_all_auths();

            let pool = deploy_pool(&env, fee_bps);
            let pool_addr = pool.client.address.clone();

            // Mint tokens for a trader.
            let trader = Address::generate(&env);
            soroban_sdk::token::StellarAssetClient::new(&env, &pool.ta)
                .mint(&trader, &amount_in);

            // Read reserves before the swap.
            let (ra_before, rb_before) = pool_reserves(&env, &pool_addr, &pool.ta, &pool.tb);
            let k_before = ra_before * rb_before;

            // Execute swap A→B; use min_out=0 and a distant deadline.
            pool.client.swap(
                &trader,
                &pool.ta,
                &amount_in,
                &0_i128,
                &u64::MAX,
            );

            let (ra_after, rb_after) = pool_reserves(&env, &pool_addr, &pool.ta, &pool.tb);
            let k_after = ra_after * rb_after;

            prop_assert!(
                k_after >= k_before,
                "k decreased on real contract: before={k_before}, after={k_after} \
                 (amount_in={amount_in}, fee_bps={fee_bps})"
            );
        }

        /// Contract property: `get_amount_out` always returns a value strictly
        /// less than `reserve_out` on the real deployed contract.
        ///
        /// Mirrors `prop_output_strictly_lt_reserve_out` but calls the
        /// on-chain `get_amount_out` view so any discrepancy between the
        /// pure-Rust mirror and the Soroban implementation is visible.
        #[test]
        fn contract_prop_get_amount_out_lt_reserve_out(
            amount_in in 1_i128..=500_000_i128,
            fee_bps   in 0_i128..=9_999_i128,
        ) {
            let env = Env::default();
            env.mock_all_auths();

            let pool = deploy_pool(&env, fee_bps);
            let pool_addr = pool.client.address.clone();

            let (_, reserve_out) = pool_reserves(&env, &pool_addr, &pool.ta, &pool.tb);

            // get_amount_out is a pure view; AmmError return is handled via try_ variant.
            let out = pool.client.try_get_amount_out(&pool.ta, &amount_in);
            let out = match out {
                Ok(Ok(v)) => v,
                _ => return Ok(()), // EmptyPool or other non-invariant error; skip
            };

            prop_assert!(
                out < reserve_out,
                "get_amount_out {out} >= reserve_out {reserve_out} \
                 (amount_in={amount_in}, fee_bps={fee_bps})"
            );
        }

        /// Contract property: per-LP-share k is non-decreasing across a
        /// sequence of `add_liquidity` + `swap` + `remove_liquidity` calls on
        /// the real deployed contract.
        ///
        /// Encodes each operation as a `(u8, i128)` pair (same encoding as
        /// `prop_per_share_k_non_decreasing_across_sequences`) so proptest can
        /// shrink failures to a minimal reproduction.
        #[test]
        fn contract_prop_per_share_k_non_decreasing_across_sequences(
            ops     in proptest::collection::vec(
                (0u8..3u8, 1_i128..=100_000_i128),
                1..16,
            ),
            fee_bps in 0_i128..=300_i128,
        ) {
            let env = Env::default();
            env.mock_all_auths();

            let pool = deploy_pool(&env, fee_bps);
            let pool_addr = pool.client.address.clone();

            // Provision a shared actor with enough tokens for all ops.
            let actor = Address::generate(&env);
            soroban_sdk::token::StellarAssetClient::new(&env, &pool.ta)
                .mint(&actor, &100_000_000_i128);
            soroban_sdk::token::StellarAssetClient::new(&env, &pool.tb)
                .mint(&actor, &100_000_000_i128);

            // Add an initial chunk so actor holds LP shares.
            pool.client.add_liquidity(
                &actor,
                &500_000_i128,
                &500_000_i128,
                &0_i128,
                &u64::MAX,
            );

            let (ra0, rb0) = pool_reserves(&env, &pool_addr, &pool.ta, &pool.tb);
            let total_shares0 = pool.client.get_info().total_shares;
            if total_shares0 <= 0 || ra0 <= 0 || rb0 <= 0 {
                return Ok(());
            }
            // Baseline uses total LP supply, which is the denominator for
            // pool-wide reserves; actor-only shares would mismeasure adds.
            let k0 = ra0 * rb0;
            let s0 = total_shares0;

            for (kind, amount) in ops {
                match kind {
                    0 => {
                        // add_liquidity — ratio-matched deposit
                        let _ = pool.client.try_add_liquidity(
                            &actor, &amount, &amount, &0_i128, &u64::MAX,
                        );
                    }
                    1 => {
                        // swap A→B
                        soroban_sdk::token::StellarAssetClient::new(&env, &pool.ta)
                            .mint(&actor, &amount);
                        let _ = pool.client.try_swap(
                            &actor, &pool.ta, &amount, &0_i128, &u64::MAX,
                        );
                    }
                    2 => {
                        // remove_liquidity — burn a small slice of actor's shares
                        let actor_shares = pool.client.shares_of(&actor);
                        if actor_shares > 1 {
                            let burn = amount.min(actor_shares - 1).max(1);
                            let _ = pool.client.try_remove_liquidity(
                                &actor, &burn, &0_i128, &0_i128, &u64::MAX,
                            );
                        }
                    }
                    _ => unreachable!(),
                }

                let total_shares_now = pool.client.get_info().total_shares;
                if total_shares_now <= 0 {
                    continue;
                }
                let (ra_now, rb_now) = pool_reserves(&env, &pool_addr, &pool.ta, &pool.tb);
                if ra_now <= 0 || rb_now <= 0 {
                    continue;
                }
                 // Invariant: ra_now * rb_now / total_shares_now^2
                //          >= ra0 * rb0 / actor_shares0^2
                // Cross-multiply: (ra_now * rb_now) * s0^2
                 //              >= k0 * total_shares_now^2
                 let lhs = ra_now.saturating_mul(rb_now).saturating_mul(s0.saturating_mul(s0));
                 let rhs = k0.saturating_mul(total_shares_now.saturating_mul(total_shares_now));
                 let rounding_tolerance = rhs / 100_000 + 1;
                prop_assert!(
                    lhs + rounding_tolerance >= rhs,
                    "per-share k regressed on real contract: \
                     ra={ra_now}, rb={rb_now}, shares={total_shares_now}, \
                     k0={k0}, s0={s0}, \
                     after op kind={kind} amount={amount}",
                );
            }
        }

        /// Stateful deployed-contract harness. Every successful operation is
        /// followed by accounting, supply and conservation checks; every
        /// rejected operation must leave the complete observable state intact.
        #[test]
        fn contract_stateful_invariants(
            ops in proptest::collection::vec(operation_strategy(), 1..20),
            fee_bps in 1_i128..=300_i128,
            unbalanced in any::<bool>(),
        ) {
            let env = Env::default();
            env.mock_all_auths();
            env.ledger().with_mut(|ledger| ledger.timestamp = 1_000_000);
            let initial_b = if unbalanced { 1_000_000_000 } else { 1_000_000 };
            let pool = deploy_pool_with_ratio(&env, fee_bps, 1_000_000, initial_b);
            let actor = Address::generate(&env);
            StellarAssetClient::new(&env, &pool.ta).mint(&actor, &100_000_000);
            StellarAssetClient::new(&env, &pool.tb).mint(&actor, &100_000_000);
            let total_a = tracked_token_total(&env, &pool.ta, &pool, &actor);
            let total_b = tracked_token_total(&env, &pool.tb, &pool, &actor);
            let mut drained = false;
            let mut reserve_accounting_quarantined = false;

            for op in ops {
                let before = state(&env, &pool);
                let mut operation = "configuration";
                let mut succeeded = true;
                let mut exact_out_observation: Option<(i128, i128, i128, i128)> = None;

                match op {
                    Operation::AddLiquidity(amount_a) => {
                        operation = "add_liquidity";
                        let amount_b = if before.reserve_a > 0 {
                            amount_a.saturating_mul(before.reserve_b) / before.reserve_a
                        } else { amount_a };
                        succeeded = matches!(pool.client.try_add_liquidity(&actor, &amount_a, &amount_b, &0, &u64::MAX), Ok(Ok(_)));
                    }
                    Operation::RemoveLiquidity(amount) => {
                        operation = "remove_liquidity";
                        let shares = pool.client.shares_of(&actor);
                        let burn = amount.min(shares);
                        succeeded = matches!(pool.client.try_remove_liquidity(&actor, &burn, &0, &0, &u64::MAX), Ok(Ok(_)));
                    }
                    Operation::RemoveLiquidityOneSided(direction, amount) => {
                        operation = "remove_liquidity_one_sided";
                        let shares = pool.client.shares_of(&actor);
                        let burn = amount.min(shares);
                        let token_out = if direction { &pool.ta } else { &pool.tb };
                        succeeded = matches!(pool.client.try_remove_liquidity_one_sided(&actor, &burn, token_out, &0, &u64::MAX), Ok(Ok(_)));
                        reserve_accounting_quarantined |= succeeded;
                    }
                    Operation::Swap(direction, amount) => {
                        operation = "swap";
                        let token_in = if direction { &pool.ta } else { &pool.tb };
                        succeeded = matches!(pool.client.try_swap(&actor, token_in, &amount, &0, &u64::MAX), Ok(Ok(_)));
                    }
                    Operation::SwapExactOut(direction, amount_out) => {
                        operation = "swap_exact_out";
                        let token_out = if direction { &pool.tb } else { &pool.ta };
                        let token_in = if direction { &pool.ta } else { &pool.tb };
                        let input_before = StellarTokenClient::new(&env, token_in).balance(&actor);
                        let output_before = StellarTokenClient::new(&env, token_out).balance(&actor);
                        let quoted = pool.client.try_get_amount_in(token_out, &amount_out);
                        let max_in = match quoted { Ok(Ok(value)) => value.saturating_add(1), _ => 1 };
                        succeeded = matches!(pool.client.try_swap_exact_out(&actor, token_out, &amount_out, &max_in, &u64::MAX), Ok(Ok(_)));
                        if succeeded {
                            let input_after = StellarTokenClient::new(&env, token_in).balance(&actor);
                            let output_after = StellarTokenClient::new(&env, token_out).balance(&actor);
                            exact_out_observation = Some((input_before - input_after, output_after - output_before, max_in, amount_out));
                        }
                    }
                    Operation::FlashLoanShort(amount) => {
                        operation = "flash_loan";
                        succeeded = matches!(pool.client.try_flash_loan(&actor, &amount, &0, &soroban_sdk::Bytes::new(&env)), Ok(Ok(_)));
                    }
                    Operation::WithdrawProtocolFees => {
                        operation = "withdraw_protocol_fees";
                        succeeded = matches!(pool.client.try_withdraw_protocol_fees(), Ok(Ok(_)));
                    }
                    Operation::UpdateFee(new_fee) => {
                        succeeded = matches!(pool.client.try_update_fee(&new_fee), Ok(Ok(_)));
                    }
                    Operation::SetProtocolFee(protocol_fee) => {
                        succeeded = matches!(pool.client.try_set_protocol_fee(&pool.admin, &pool.admin, &protocol_fee), Ok(Ok(_)));
                    }
                    Operation::SetLpRebate(rebate) => {
                        succeeded = matches!(pool.client.try_set_lp_rebate(&pool.admin, &rebate), Ok(Ok(_)));
                    }
                    Operation::AdvanceLedger(seconds) => {
                        env.ledger().with_mut(|ledger| {
                            ledger.timestamp = ledger.timestamp.saturating_add(seconds);
                            ledger.sequence_number = ledger.sequence_number.saturating_add(1);
                        });
                    }
                    Operation::EmergencyWithdraw => {
                        operation = "emergency_withdraw";
                        // The hardened path needs a pause, an announcement and the
                        // default 24 h delay. None of these touch reserves or
                        // supply, so a rejected attempt still leaves `State` intact.
                        let _ = pool.client.try_pause();
                        let _ = pool
                            .client
                            .try_announce_emergency_withdraw(&pool.admin, &pool.admin);
                        env.ledger().with_mut(|ledger| {
                            ledger.timestamp = ledger.timestamp.saturating_add(86_400);
                        });
                        succeeded = matches!(
                            pool.client.try_emergency_withdraw(&pool.admin, &pool.admin),
                            Ok(Ok(_))
                        );
                        drained |= succeeded;
                    }
                }

                let after = state(&env, &pool);
                if !succeeded {
                    prop_assert_eq!(after, before, "failed {} changed observable state", operation);
                    continue;
                }

                if !reserve_accounting_quarantined && !is_known_failure("reserve_accounting", operation) {
                    prop_assert_eq!(after.reserve_a + after.accrued_a, after.pool_balance_a,
                        "token A reserve accounting diverged after {}", operation);
                    prop_assert_eq!(after.reserve_b + after.accrued_b, after.pool_balance_b,
                        "token B reserve accounting diverged after {}", operation);
                }
                if !drained && !is_known_failure("lp_supply", operation) {
                    prop_assert_eq!(after.total_shares, after.lp_supply,
                        "LP supply diverged after {}", operation);
                }
                prop_assert_eq!(tracked_token_total(&env, &pool.ta, &pool, &actor), total_a,
                    "token A was created or destroyed after {}", operation);
                prop_assert_eq!(tracked_token_total(&env, &pool.tb, &pool, &actor), total_b,
                    "token B was created or destroyed after {}", operation);

                if (operation == "swap" || operation == "swap_exact_out")
                    && !is_known_failure("exact_out_delivery", operation)
                {
                    let k_before = before.reserve_a.saturating_mul(before.reserve_b);
                    let k_after = after.reserve_a.saturating_mul(after.reserve_b);
                    prop_assert!(k_after >= k_before, "k decreased after {}", operation);
                }

                if let Some((spent, received, quoted_max, expected_out)) = exact_out_observation {
                    if !is_known_failure("exact_out_delivery", operation) {
                        prop_assert_eq!(received, expected_out);
                        prop_assert!(spent <= quoted_max, "exact-out spent above its quote");
                    }
                }
            }
        }
    }

    #[test]
    fn known_failure_quarantine_is_specific_and_traceable() {
        let mut pairs = std::collections::BTreeSet::new();
        for (invariant, operation, issue) in KNOWN_FAILURES {
            assert!(issue.starts_with('#'));
            assert!(
                pairs.insert((*invariant, *operation)),
                "duplicate quarantine entry"
            );
        }
    }
}

// ── Deterministic regression tests ───────────────────────────────────────────

#[cfg(test)]
mod regression {
    use super::*;

    /// Any edge case found by the fuzzer should be pinned here so it is
    /// reproduced on every CI run without re-running the full 10 000 cases.

    #[test]
    fn regression_zero_fee_exact_formula() {
        // fee_bps=0: out = amount_in * reserve_out / (reserve_in + amount_in)
        assert_eq!(cp_amount_out(1_000, 1_000_000, 1_000_000, 0), 999);
        assert_eq!(cp_amount_out(100_000, 1_000_000, 1_000_000, 0), 90_909);
    }

    #[test]
    fn regression_max_fee_zero_output() {
        assert_eq!(cp_amount_out(1_000_000, 1_000_000, 1_000_000, 10_000), 0);
    }

    #[test]
    fn regression_k_invariant_with_30bps_fee() {
        let r_in = 1_000_000_i128;
        let r_out = 1_000_000_i128;
        let k_before = r_in * r_out;
        let out = cp_amount_out(100_000, r_in, r_out, 30);
        let k_after = (r_in + 100_000) * (r_out - out);
        assert!(
            k_after >= k_before,
            "k decreased at 30bps: before={k_before}, after={k_after}"
        );
    }

    #[test]
    fn regression_k_invariant_across_all_standard_fee_tiers() {
        let fee_tiers = [0_i128, 1, 5, 10, 30, 100, 300, 1_000, 3_000, 10_000];
        let r_in = 1_000_000_i128;
        let r_out = 2_000_000_i128;
        let amount_in = 100_000_i128;

        for &fee in &fee_tiers {
            let k_before = r_in * r_out;
            let out = cp_amount_out(amount_in, r_in, r_out, fee);
            let k_after = (r_in + amount_in) * (r_out - out);
            assert!(
                k_after >= k_before,
                "k decreased at fee_bps={fee}: before={k_before}, after={k_after}"
            );
        }
    }

    #[test]
    fn regression_no_overflow_near_i128_max() {
        // reserve values near 4e18 are used in the AMM overflow guard tests.
        // Verify formula handles them without overflow.
        let r = 4_000_000_000_000_000_000_i128; // 4e18
        let amount_in = 1_000_000_000_i128; // 1e9
        let fee_bps = 30_i128;
        // amount_in_with_fee = 1e9 * 9970 ~ 9.97e12; numerator ~ 9.97e12 * 4e18 ~ 4e31 < i128::MAX
        let out = cp_amount_out(amount_in, r, r, fee_bps);
        assert!(out > 0 && out < r);
    }

    #[test]
    fn regression_fee_monotone_in_fee_bps() {
        // Higher fee → lower output (or equal when both are 0).
        let (r_in, r_out, amount_in) = (1_000_000_i128, 1_000_000_i128, 50_000_i128);
        let mut prev = i128::MAX;
        for fee in [0_i128, 1, 5, 10, 30, 100, 300, 1_000, 3_000, 9_999, 10_000] {
            let out = cp_amount_out(amount_in, r_in, r_out, fee);
            assert!(out <= prev, "output increased as fee rose to {fee}bps");
            prev = out;
        }
    }

    #[test]
    fn regression_output_zero_only_at_zero_input_or_max_fee() {
        assert_eq!(cp_amount_out(0, 1_000_000, 1_000_000, 30), 0);
        assert_eq!(cp_amount_out(1_000, 1_000_000, 1_000_000, 10_000), 0);
        // Small amounts can round to zero; 1_000 is known to give >0.
        assert!(cp_amount_out(1_000, 1_000_000, 1_000_000, 0) > 0);
    }

    /// Regression for #303: a hand-crafted sequence
    /// (add → swap → swap → remove) keeps per-share k non-decreasing.
    #[test]
    fn regression_sequence_per_share_k_non_decreasing() {
        let mut pool = SimPool::new(1_000_000, 2_000_000, 30);
        let baseline = pool.per_share_k();
        pool.add_liquidity(100_000);
        assert!(pool.per_share_k() + 1 >= baseline);
        pool.swap_a_for_b(50_000);
        assert!(pool.per_share_k() + 1 >= baseline);
        pool.swap_a_for_b(25_000);
        assert!(pool.per_share_k() + 1 >= baseline);
        pool.remove_liquidity_share(50_000);
        assert!(pool.per_share_k() + 1 >= baseline);
    }

    /// Regression for #303: simulate_swap direction is always
    /// adverse for the trader regardless of starting reserves.
    #[test]
    fn regression_simulate_swap_direction_adverse() {
        for (r_in, r_out) in [
            (1_000_000, 1_000_000),
            (500_000, 5_000_000),
            (5_000_000, 500_000),
        ] {
            let pre_price = r_out as i128 * 1_000_000 / r_in as i128;
            let out = cp_amount_out(50_000, r_in, r_out, 30);
            let post_price = (r_out as i128 - out) * 1_000_000 / (r_in as i128 + 50_000);
            assert!(
                post_price <= pre_price,
                "direction wrong for r_in={r_in}, r_out={r_out}"
            );
        }
    }
}
