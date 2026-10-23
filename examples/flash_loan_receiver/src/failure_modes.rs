//! Educational guide to flash loan failure modes.
//!
//! This module documents common mistakes that lead to lost funds or reverted
//! transactions when working with flash loans. These are pseudo-code examples
//! showing the PATTERN to avoid, not compilable contracts.
//!
//! ## Failure modes covered
//!
//! 1. **Insufficient profit**: Borrow, attempt a trade that doesn't profit enough
//!    to cover the fee. The receiver detects this and returns `false`, causing
//!    the entire transaction to revert. The pool is unharmed.
//!
//!    ```ignore
//!    // Borrower: Okay, I borrowed 100 XLM, fee is 5 XLM.
//!    // I traded for 102 XLM elsewhere but costs 5 XLM to do.
//!    // Net = 102 - 105 = -3 XLM loss, so return false.
//!    if profit < fee {
//!        return false; // Revert entire flash loan
//!    }
//!    ```
//!
//! 2. **Reentrancy attempt**: Try to call the pool during the flash loan callback.
//!    The pool's reentrancy guard rejects this immediately with `Reentrant` error.
//!
//!    ```ignore
//!    // During callback, attempting to call pool again:
//!    let price = pool.get_price();  // ❌ ERROR: Reentrant!
//!    ```
//!
//! 3. **Incomplete repayment**: Repay the principal but not the fee. The pool's
//!    balance check fails before the callback returns, reverting the transaction.
//!
//!    ```ignore
//!    // Borrowed 100, fee 5, but only repay 100:
//!    token.transfer(&pool, 100);  // ❌ Pool expects 105
//!    ```
//!
//! 4. **Wrong recipient**: Repay to an address other than the pool. The pool does
//!    not receive the funds and reverts.
//!
//!    ```ignore
//!    // Repay to wrong address:
//!    token.transfer(&wrong_addr, 105);  // ❌ Pool not paid
//!    ```
//!
//! ## Common patterns that lead to each failure
//!
//! These are demonstrated below as plain functions rather than deployable
//! `#[contract]` types: a single crate's wasm export table is shared across
//! all contracts it defines, so multiple `on_flash_loan`/`initialize`
//! entry points here would collide with each other and with
//! [`crate::FlashLoanReceiver`].

use soroban_sdk::{Address, Bytes, Env};

/// Demonstrates a flash loan callback that aborts because profit is
/// insufficient to cover the fee.
///
/// Returns `false` to signal the strategy failed, causing the entire
/// flash loan to revert atomically.
///
/// # Demonstration
///
/// When this returns `false`, the pool reverts the entire transaction:
/// - Token transfers are rolled back
/// - The receiver gains nothing
/// - The caller receives no funds
pub fn insufficient_profit_on_flash_loan(
    _env: Env,
    _token_a_amount: i128,
    _token_b_amount: i128,
    _fee_a: i128,
    _fee_b: i128,
    _data: Bytes,
) -> bool {
    // Simulate checking the trade opportunity
    let profit_bps = 10; // 0.1% profit (way too low)
    let fee_bps = 50; // 0.5% fee

    // Abort if profit doesn't cover the fee
    if profit_bps < fee_bps {
        return false; // Pool reverts the entire flash loan
    }

    true
}

/// Demonstrates a flash loan callback that attempts to reenter the pool.
///
/// The pool's reentrancy guard rejects this with error `Reentrant`.
/// The entire transaction reverts, and the receiver gains nothing.
pub fn reentrancy_attempt_on_flash_loan(
    env: Env,
    pool: &Address,
    token_a_amount: i128,
    token_b_amount: i128,
    fee_a: i128,
    fee_b: i128,
    _data: Bytes,
) -> bool {
    // WRONG: Attempting to call the pool during the callback
    //
    // Soroban AMM's reentrancy guard will reject this with error 14 (Reentrant).
    // This line would cause a revert if uncommented:
    //
    // let client = soroban_amm_sdk::client::AmmPoolClient::new(&env, pool);
    // let _info = client.get_info(); // <- This will be rejected by the guard
    //
    // Instead, we just demonstrate the structure:

    let receiver = env.current_contract_address();
    let _ = pool;

    // To properly handle repayment, we'd need the pool info, which we cannot
    // get during the callback. This is the cost of the reentrancy guard —
    // you must know token addresses beforehand or pass them via `data`.
    //
    // In a real scenario, you'd pass token addresses in the `data` parameter
    // and decode them here.

    // Simulate the repayment (even though we can't actually do it in this demo)
    if token_a_amount > 0 || fee_a > 0 {
        // TokenClient::new(&env, &info.token_a).transfer(
        //     &receiver,
        //     pool,
        //     &(token_a_amount + fee_a),
        // );
    }

    if token_b_amount > 0 || fee_b > 0 {
        // TokenClient::new(&env, &info.token_b).transfer(
        //     &receiver,
        //     pool,
        //     &(token_b_amount + fee_b),
        // );
    }

    let _ = receiver;
    true
}

/// Demonstrates a flash loan callback that repays principal but not the fee.
///
/// The pool's post-callback balance check will detect the shortfall
/// and revert the entire transaction before this function returns.
pub fn incomplete_repayment_on_flash_loan(
    env: Env,
    pool: &Address,
    token_a_amount: i128,
    token_b_amount: i128,
    fee_a: i128, // This is NOT repaid
    fee_b: i128, // This is NOT repaid
    _data: Bytes,
) -> bool {
    let receiver = env.current_contract_address();
    let _ = (pool, receiver, token_a_amount, token_b_amount);

    // WRONG: Only repay principal, omit the fee
    //
    // if token_a_amount > 0 {
    //     TokenClient::new(&env, &info.token_a).transfer(
    //         &receiver,
    //         pool,
    //         &token_a_amount, // <- Fee is missing!
    //     );
    // }
    //
    // The pool's post-callback check will detect the shortfall:
    //     balance_after < balance_before + amount + fee
    // and revert with FlashLoanRepaymentFailed (error code 18).

    let _ = (fee_a, fee_b); // Unused in this wrong implementation

    false // Abort to avoid this error in the test
}

/// Demonstrates a flash loan callback that repays to the wrong address.
///
/// The pool's post-callback balance check will detect that the pool did not
/// receive the repayment and revert.
pub fn wrong_recipient_on_flash_loan(
    env: Env,
    pool: &Address,
    token_a_amount: i128,
    token_b_amount: i128,
    fee_a: i128,
    fee_b: i128,
    _data: Bytes,
) -> bool {
    let receiver = env.current_contract_address();
    let _ = pool;

    // WRONG: Repay to the receiver instead of the pool
    //
    // let wrong_recipient = receiver.clone(); // Should be pool, not receiver
    // if token_a_amount > 0 || fee_a > 0 {
    //     TokenClient::new(&env, &info.token_a).transfer(
    //         &receiver,
    //         &wrong_recipient, // <- Wrong address!
    //         &(token_a_amount + fee_a),
    //     );
    // }
    //
    // The pool's post-callback balance check will show:
    //     balance_after (unchanged) < balance_before + amount + fee
    // and revert.

    let _ = (receiver, token_a_amount, token_b_amount, fee_a, fee_b);
    false // Abort to avoid this error in the test
}
