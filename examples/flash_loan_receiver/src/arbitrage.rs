//! Arbitrage strategy: borrow tokens, trade them at a better price, repay the loan.
//!
//! ## How it works
//!
//! Suppose pool A trades token X for token Y at price 1.0 (1 X = 1 Y), but
//! pool B trades them at price 0.95 (1 X = 0.95 Y, meaning X is cheaper at B).
//!
//! An arbitrageur can:
//! 1. Flash borrow 1000 X from pool A
//! 2. Sell those 1000 X on pool B, receiving ~950 Y
//! 3. Use 1000 Y of that to cover the principal owed to pool A
//! 4. Pocket the ~50 Y difference (minus flash loan fee)
//!
//! If the price difference closes before the strategy executes (e.g., another
//! arbitrageur got there first), the strategy must detect this and abort cleanly
//! rather than losing money.
//!
//! ## Economics
//!
//! **Profit formula:**
//!
//! ```text
//! profit = (amount_in / spot_price_worse) * spot_price_better - amount_in - flash_loan_fee
//! ```
//!
//! Where:
//! - `amount_in`: Size of the arbitrage (e.g., 1000 tokens)
//! - `spot_price_worse`: Price at the worse market (where we trade)
//! - `spot_price_better`: Price at the better market (where we borrow)
//! - `flash_loan_fee`: Fee charged by the pool (typically 0.1-0.5 bps)
//!
//! The arbitrage is profitable only if the profit is strictly positive.
//! Real arbitrage typically runs on microsecond timescales; a slow receiver
//! implementation may miss the window.

use soroban_sdk::Env;

/// Arbitrage configuration.
///
/// Specifies the borrowing pool, the destination pool for the counter-trade,
/// and the minimum profit required to execute.
#[derive(Clone, Debug)]
pub struct ArbitrageConfig {
    /// Pool to borrow from
    pub borrow_pool: soroban_sdk::Address,
    /// Pool to trade at
    pub counter_pool: soroban_sdk::Address,
    /// Minimum profit in the counter token (0 = accept any profit)
    pub min_profit: i128,
}

/// Execute an arbitrage by trading borrowed tokens at a better price elsewhere.
///
/// # Security Invariant
///
/// This strategy entry point is only intended to be called from within the verified
/// [`crate::FlashLoanReceiver::on_flash_loan`] callback after pool caller authentication
/// (`pool.require_auth()`). Never invoke strategy logic or transfer borrowed funds
/// directly from unauthenticated public functions.
///
/// This is a skeleton implementation demonstrating the flow. A real implementation
/// would:
/// 1. Check if the opportunity still exists at `counter_pool`
/// 2. Execute the counter-trade if profitable
/// 3. Return `false` if the opportunity closed
///
/// # Arguments
/// - `env`: Soroban environment
/// - `borrowed_amount`: Amount borrowed from the primary pool
/// - `config`: Arbitrage configuration
///
/// # Returns
/// - `Ok(profit)` if successful (profit in counter token)
/// - `Err(_)` if the opportunity closed or execution failed
pub fn execute_arbitrage(
    _env: &Env,
    _borrowed_amount: i128,
    _config: &ArbitrageConfig,
) -> Result<i128, &'static str> {
    // Skeleton: real implementation would:
    // 1. Call counter_pool.simulate_swap to check if arbitrage is still profitable
    // 2. If profitable, call counter_pool.swap to execute the counter-trade
    // 3. Calculate profit and return it
    // 4. If price moved unfavorably, return Err to abort
    //
    // See tests/flash_loan_test.rs for a fully worked example.

    Err("Arbitrage opportunity closed")
}
