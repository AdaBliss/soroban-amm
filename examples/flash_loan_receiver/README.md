# Flash Loan Receiver Reference Implementation

A reference implementation of a Soroban AMM flash loan receiver, demonstrating safe usage patterns, authentication requirements, common pitfalls, and testing strategies.

## What is a flash loan?

A flash loan is a loan that is borrowed and repaid within a single atomic transaction. The borrower receives funds, can use them for any purpose, but must return the principal plus a fee before the transaction completes.

**Flow:**

1. Caller invokes `pool.flash_loan(receiver, amount_a, amount_b, data)`
2. Pool transfers tokens to the receiver contract
3. Pool calls `receiver.on_flash_loan(token_a_amount, token_b_amount, fee_a, fee_b, data)`
4. Receiver must transfer back `amount_a + fee_a` and `amount_b + fee_b` to the pool
5. Pool verifies repayment
6. Transaction commits (or reverts if repayment incomplete)

Flash loans are powerful for arbitrage, liquidations, and collateral swaps — but require careful implementation. Getting the repayment or reentrancy guard wrong costs all borrowed funds.

## Callback contract guarantees

The callback signature matches the AMM pool interface:

```rust
pub trait FlashLoanReceiver {
    fn on_flash_loan(
        env: Env,
        token_a_amount: i128,
        token_b_amount: i128,
        fee_a: i128,
        fee_b: i128,
        data: Bytes,
    ) -> bool;
}
```

### What the pool guarantees

- Your `on_flash_loan` is called exactly once
- Token balances are increased before the callback
- The pool does not check repayment until your callback returns
- Reentrancy into the pool is rejected by a guard

### What you must guarantee

- **Before returning**, transfer back to the pool:
    - `amount_a + fee_a` of token A
    - `amount_b + fee_b` of token B
- **Missing the repayment** → entire transaction reverts with `FlashLoanRepaymentFailed` (error code 18)
- **Partial repayment** → entire transaction reverts with `FlashLoanRepaymentFailed` (error code 18)
- **Attempting reentrancy** → transaction reverts with `Reentrant` (error code 14)

## Security checklist for receivers

When designing or integrating a flash loan receiver, follow these essential security rules:

1. **Authenticate the pool caller**:
   In `on_flash_loan`, always verify that the direct caller is the configured pool contract by calling `pool.require_auth()`. Without this check, any stranger can invoke your callback directly with arbitrary amounts and drain funds held by the receiver into the pool.
2. **Once-only initialization (`initialize`)**:
   Ensure `initialize` cannot be called more than once. If re-initialization is allowed, an attacker can re-point your receiver to a malicious pool contract and siphon receiver funds.
3. **Deployment-window caveat**:
   Because `initialize` is typically unauthenticated on initial deployment, immediately verify the initialization parameters (e.g. read back the stored pool address and owner) or execute deployment and initialization atomically within the same transaction envelope.
4. **No unsolicited-call funds movement**:
   Never move funds or execute trading strategies in response to unsolicited calls. Only move assets when invoked through the authenticated pool callback.
5. **Repay-or-revert**:
   Ensure all execution branches guarantee either full repayment (`principal + fee`) or an explicit revert/abort (`return false` or panic).
6. **Rescue function for stranded funds**:
   Provide an owner-authenticated `rescue(token, amount, to)` function allowing recovery of trapped tokens, protected by `owner.require_auth()`.
7. **Avoid storing keys or secrets**:
   Do not store private signing keys or sensitive configuration on-chain.

## Strategies implemented

### Arbitrage

Borrow at one pool, trade at a better price elsewhere, repay the loan.

```
Borrow 1000 X from pool A → Sell at pool B (better price) → Repay pool A + fee → Keep profit
```

**Requirements:**

- Identify price differences between markets
- Execute the counter-trade before the callback returns
- Profit must exceed the flash loan fee, or the entire attempt fails

**File:** [`src/arbitrage.rs`](src/arbitrage.rs)

### Collateral swap / Liquidity migration

Migrate an LP position from one pool to another without intermediate price exposure.

```
Borrow underlying tokens from pool B → Add liquidity to pool B
→ Remove liquidity from pool A → Repay pool B + fee → Profit from structural change
```

**Requirements:**

- Coordinate two or more pool operations atomically
- Ensure repayment is possible from reclaimed collateral
- Handle slippage correctly on both sides

**File:** [`src/collateral_swap.rs`](src/collateral_swap.rs)

## Failure modes demonstrated

Every pitfall is implemented and tested so you can see exactly what happens:

### 1. Insufficient profit aborts cleanly

```rust
fn on_flash_loan(...) -> bool {
    // Check if the opportunity is still profitable
    if profit_bps < fee_bps {
        return false; // Abort; pool reverts the entire flash loan
    }
    // ... continue with profitable trade
}
```

The receiver returns `false`, the pool reverts atomically, and no funds are lost.

**Test:** [`tests/flash_loan_test.rs::insufficient_profit_aborts_cleanly`](tests/flash_loan_test.rs)

### 2. Reentrancy is blocked

```rust
fn on_flash_loan(...) -> bool {
    // Attempting to call the pool during callback triggers the reentrancy guard:
    let client = AmmPoolClient::new(&env, &pool);
    let info = client.get_info(); // <- REJECTED: Reentrant error (code 14)
}
```

The pool's reentrancy guard rejects any pool call during the callback.

**Workaround:** Cache token addresses at `initialize` time or pass them in the `data` parameter.

**Test:** [`tests/flash_loan_test.rs::reentrancy_attempt_rejected`](tests/flash_loan_test.rs)

### 3. Incomplete repayment reverts

```rust
fn on_flash_loan(env, amount_a, amount_b, fee_a, fee_b, _data) -> bool {
    TokenClient::new(&env, token_a).transfer(
        &receiver,
        &pool,
        &amount_a, // <- WRONG: fee_a is missing!
    );
    true
}
// Pool post-check: balance_after < balance_before + amount + fee
// Result: FlashLoanRepaymentFailed (code 18) error, entire transaction reverts
```

The pool checks total repayment after the callback returns. Shortfall = revert.

**Test:** [`tests/flash_loan_test.rs::incomplete_repayment_reverts`](tests/flash_loan_test.rs)

### 4. Repayment to the wrong address reverts

```rust
fn on_flash_loan(...) -> bool {
    TokenClient::new(&env, token_a).transfer(
        &receiver,
        &wrong_address, // <- Not the pool!
    );
    true
}
// Pool post-check: balance_after < balance_before + amount + fee
// Result: FlashLoanRepaymentFailed (code 18) error, entire transaction reverts
```

The pool checks its own balance, not whether a transfer happened. Send to the wrong
address, the pool doesn't see it, and the transaction reverts.

**Test:** [`tests/flash_loan_test.rs::wrong_recipient_reverts`](tests/flash_loan_test.rs)

## Testing strategy

The test suite in [`tests/flash_loan_test.rs`](tests/flash_loan_test.rs) uses a real Soroban
test environment with actual contracts:

- **No mocks** — every test deploys real AMM and token contracts
- **No network calls** — everything runs in-process with recorded ledger state
- **Recorded state** — ledger timestamp, sequence number, and all balances are deterministic

### Test categories

**Happy path** (10+ tests):

- Repay principal + fee exactly
- Repay both tokens correctly
- Fee accounting is exact across multiple loans
- Token conservation

**Failure modes** (4+ tests):

- Insufficient profit aborts cleanly
- Reentrancy is rejected
- Incomplete repayment reverts
- Wrong recipient reverts

Run the full suite:

```bash
cargo test -p flash_loan_receiver
```

## Deployment guide

### Step 1: Compile to WASM

```bash
cargo build --release --target wasm32v1-none -p flash_loan_receiver
```

Output: `target/wasm32v1-none/release/flash_loan_receiver.wasm`

### Step 2: Deploy the contract

Using `soroban-cli`:

```bash
soroban contract deploy \
  --wasm target/wasm32v1-none/release/flash_loan_receiver.wasm \
  --network testnet \
  --source <your-key>
```

This returns the contract address.

### Step 3: Initialize the receiver

Call the `initialize` entry point with the pool address and owner address:

```bash
soroban contract invoke \
  --id <receiver-address> \
  --network testnet \
  --source <your-key> \
  -- initialize \
  --pool <pool-address> \
  --owner <owner-address>
```

> **Note on Deployment Window**: `initialize` is one-shot. Read back the configured pool and owner after invoking `initialize` to ensure it was not initialized by an unauthorized party during the deployment window.

### Step 4: Trigger a flash loan

From another contract or off-chain via a transaction envelope:

```bash
soroban contract invoke \
  --id <pool-address> \
  --network testnet \
  --source <your-key> \
  -- flash_loan \
  --receiver <receiver-address> \
  --amount_a 1000000 \
  --amount_b 0 \
  --data ""
```

## Fee structure

The pool charges a flash loan fee as a percentage of the borrowed amount:

- **Testnet**: Typically 0.05% (5 basis points)
- **Mainnet**: May vary by pool; check `pool.get_info().flash_loan_fee_bps`

**Example:**

- Borrow 1,000,000 tokens
- Fee: 1,000,000 × 5 / 10,000 = 500 tokens
- Repayment: 1,000,500 tokens

## Reentrancy guard workaround

The pool's reentrancy guard prevents any pool function call during the callback. To work
around this, pass all required data in the `data` parameter:

```rust
fn on_flash_loan(
    env: Env,
    token_a_amount: i128,
    token_b_amount: i128,
    fee_a: i128,
    fee_b: i128,
    data: Bytes,
) -> bool {
    // Decode token addresses from `data`
    let (token_a, token_b) = decode_tokens(&data);

    // Now you can repay without calling the pool
    TokenClient::new(&env, &token_a).transfer(&receiver, &pool, &(token_a_amount + fee_a));

    true
}
```

## Common mistakes

1. **Forgetting to repay the fee** — Most common; transaction reverts
2. **Reentering the pool** — Triggers `Reentrant` error
3. **Not checking profitability** — Strategy loses money if opportunity closes
4. **Assuming tokens are synchronized** — Fee-on-transfer tokens may deduct more
5. **No deadline/timeout** — Strategy may execute after conditions change

## Related resources

- [Soroban AMM Contracts](../../contracts/amm)
- [AMM SDK](../../contracts/amm-sdk)
- [Event Schema Versioning](../../docs/event-schema-versioning.md)
- [Flash Loan Economics](https://docs.aave.com/developers/guides/flash-loans)

## License

MIT
