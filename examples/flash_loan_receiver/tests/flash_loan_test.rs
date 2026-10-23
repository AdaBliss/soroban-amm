#![cfg(test)]

use flash_loan_receiver::FlashLoanReceiver;
use soroban_amm_sdk::client::AmmPoolClient;
use soroban_amm_sdk::types::PoolInfo;
use soroban_sdk::{
    testutils::{Address as _, Ledger, LedgerInfo},
    token::{Client as TokenClient, StellarAssetClient},
    Address, BytesN, Env,
};

// WASM artifacts
use amm::WASM as AMM_WASM;

// Setup fixture
struct FlashLoanTestEnv<'a> {
    env: Env,
    pool_client: AmmPoolClient<'a>,
    token_a: Address,
    #[allow(dead_code)]
    token_b: Address,
    receiver: Address,
    #[allow(dead_code)]
    owner: Address,
    pool: Address,
}

impl FlashLoanTestEnv<'_> {
    fn new() -> Self {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        // Set ledger
        env.ledger().set(LedgerInfo {
            timestamp: 1000,
            protocol_version: 21,
            sequence_number: 1,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 16,
            min_persistent_entry_ttl: 4096,
            max_entry_ttl: 6_312_000,
        });

        let admin = Address::generate(&env);

        // Upload contract WASM
        let amm_hash: BytesN<32> = env.deployer().upload_contract_wasm(AMM_WASM);

        // Create token pair
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let lp_token = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        // Deploy AMM pool from the uploaded WASM
        let pool = env
            .deployer()
            .with_address(
                Address::generate(&env),
                BytesN::from_array(&env, &[0u8; 32]),
            )
            .deploy(amm_hash);

        let pool_client = AmmPoolClient::new(&env, &pool);
        pool_client.initialize(
            &admin, &token_a, &token_b, &lp_token, &30i128, &admin, &0i128,
        );
        // Distinct flash-loan fee (5 bps) from the swap fee (30 bps), matching
        // the fee assumptions the tests below are written against.
        pool_client.update_flash_loan_fee(&5i128);

        // Register the flash loan receiver contract
        let receiver = env.register_contract(None, FlashLoanReceiver);

        let owner = Address::generate(&env);
        let receiver_client = flash_loan_receiver::FlashLoanReceiverClient::new(&env, &receiver);
        receiver_client.initialize(&pool, &owner);

        // The reference receiver repays principal + fee unconditionally — it
        // doesn't run a real arbitrage strategy — so it needs a standing
        // balance to cover the fee side of every repayment.
        StellarAssetClient::new(&env, &token_a).mint(&receiver, &10_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&receiver, &10_000_000_i128);

        // Mint initial tokens to admin
        StellarAssetClient::new(&env, &token_a).mint(&admin, &1_000_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&admin, &1_000_000_000_i128);

        // Approve pool to spend tokens
        let token_a_client = TokenClient::new(&env, &token_a);
        let token_b_client = TokenClient::new(&env, &token_b);
        token_a_client.approve(&admin, &pool, &1_000_000_000_i128, &1_000_000);
        token_b_client.approve(&admin, &pool, &1_000_000_000_i128, &1_000_000);

        // Add initial liquidity
        pool_client.add_liquidity(&admin, &1_000_000_i128, &1_000_000_i128, &0i128, &u64::MAX);

        Self {
            env,
            pool_client,
            token_a,
            token_b,
            receiver,
            owner,
            pool,
        }
    }

    fn get_pool_info(&self) -> PoolInfo {
        self.pool_client.get_info()
    }

    fn get_token_balance(&self, token: &Address, account: &Address) -> i128 {
        TokenClient::new(&self.env, token).balance(account)
    }

    fn execute_flash_loan(&self, amount_a: i128, amount_b: i128) {
        self.pool_client.flash_loan(
            &self.receiver,
            &amount_a,
            &amount_b,
            &soroban_sdk::Bytes::new(&self.env),
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Happy path tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn happy_path_repay_principal_plus_fee() {
    let fixture = FlashLoanTestEnv::new();

    let initial_info = fixture.get_pool_info();
    let initial_reserve_a = initial_info.reserve_a;
    let initial_reserve_b = initial_info.reserve_b;

    // Execute flash loan
    let borrow_amount_a = 1_000_000i128;
    let borrow_amount_b = 0i128;
    fixture.execute_flash_loan(borrow_amount_a, borrow_amount_b);

    // Verify: pool reserves grew by exactly the fee
    let final_info = fixture.get_pool_info();

    // Flash loan fee is typically 0.05% = 5 bps
    let expected_fee_a = (borrow_amount_a * 5) / 10_000;

    // Reserves should increase by the fee
    assert_eq!(
        final_info.reserve_a,
        initial_reserve_a + expected_fee_a,
        "Pool reserve A should increase by the fee"
    );

    // Reserves should be unchanged for token B
    assert_eq!(
        final_info.reserve_b, initial_reserve_b,
        "Pool reserve B should be unchanged"
    );
}

#[test]
fn happy_path_repay_both_tokens() {
    let fixture = FlashLoanTestEnv::new();

    let initial_info = fixture.get_pool_info();
    let initial_reserve_a = initial_info.reserve_a;
    let initial_reserve_b = initial_info.reserve_b;

    // Borrow both tokens
    let borrow_amount_a = 500_000i128;
    let borrow_amount_b = 500_000i128;
    fixture.execute_flash_loan(borrow_amount_a, borrow_amount_b);

    // Verify: both reserves grew by their fees
    let final_info = fixture.get_pool_info();
    let fee_bps = 5i128; // 0.05% = 5 bps

    let expected_fee_a = (borrow_amount_a * fee_bps) / 10_000;
    let expected_fee_b = (borrow_amount_b * fee_bps) / 10_000;

    assert_eq!(
        final_info.reserve_a,
        initial_reserve_a + expected_fee_a,
        "Pool reserve A should increase by the fee"
    );

    assert_eq!(
        final_info.reserve_b,
        initial_reserve_b + expected_fee_b,
        "Pool reserve B should increase by the fee"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Edge case tests
//
// The reference `FlashLoanReceiver` always repays unconditionally — it has
// no profit check to decline a loan with (see `failure_modes.rs` for
// receivers that demonstrate declining/failing repayment instead). These
// tests cover the edge cases that *this* receiver can actually exercise.
// ═══════════════════════════════════════════════════════════════════════════

#[test]
#[should_panic]
fn zero_borrow_amount_is_rejected() {
    let fixture = FlashLoanTestEnv::new();

    // The pool rejects a flash loan that borrows nothing (`AmmError::ZeroAmount`)
    // before ever invoking the receiver's callback.
    fixture.execute_flash_loan(0, 0);
}

#[test]
fn receiver_repays_correct_amount() {
    let fixture = FlashLoanTestEnv::new();

    let borrow_amount_a = 100_000i128;
    let fee_bps = 5i128; // 0.05% = 5 bps
    let expected_fee = (borrow_amount_a * fee_bps) / 10_000;

    // Get token balance before
    let receiver_balance_before = fixture.get_token_balance(&fixture.token_a, &fixture.receiver);

    // Execute flash loan (receiver gets amount, then must repay amount + fee)
    fixture.execute_flash_loan(borrow_amount_a, 0);

    // Receiver repays the borrowed principal in full, plus the fee out of
    // its own standing balance (it isn't running a real arbitrage here).
    let receiver_balance_after = fixture.get_token_balance(&fixture.token_a, &fixture.receiver);
    assert_eq!(
        receiver_balance_after,
        receiver_balance_before - expected_fee,
        "Receiver should repay all borrowed tokens, losing only the fee"
    );

    // Pool should have gained the fee
    let pool_info = fixture.get_pool_info();
    assert!(
        pool_info.reserve_a > 1_000_000_i128,
        "Pool should gain fee from flash loan"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Fee accounting tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn fee_accounting_is_exact() {
    let fixture = FlashLoanTestEnv::new();

    let initial_info = fixture.get_pool_info();
    let initial_reserve_a = initial_info.reserve_a;

    // Kept within the pool's reserve (seeded at 1_000_000 by the fixture).
    let borrow_amount = 900_000i128;
    let fee_bps = 5i128; // 0.05% = 5 bps
    let expected_fee = (borrow_amount * fee_bps) / 10_000;

    fixture.execute_flash_loan(borrow_amount, 0);

    let final_info = fixture.get_pool_info();

    // Reserve should increase by exactly the calculated fee
    assert_eq!(
        final_info.reserve_a,
        initial_reserve_a + expected_fee,
        "Reserve should increase by exactly the flash loan fee"
    );
}

#[test]
fn multiple_flash_loans_accumulate_fees() {
    let fixture = FlashLoanTestEnv::new();

    let initial_info = fixture.get_pool_info();
    let initial_reserve_a = initial_info.reserve_a;

    let borrow_amount = 500_000i128;
    let fee_bps = 5i128;
    let expected_fee_per_loan = (borrow_amount * fee_bps) / 10_000;

    // Execute two flash loans
    fixture.execute_flash_loan(borrow_amount, 0);
    fixture.execute_flash_loan(borrow_amount, 0);

    let final_info = fixture.get_pool_info();

    // Both fees should accumulate into the reserve
    assert_eq!(
        final_info.reserve_a,
        initial_reserve_a + (expected_fee_per_loan * 2),
        "Reserve should include both flash loan fees"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Token conservation tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn total_tokens_are_conserved() {
    let fixture = FlashLoanTestEnv::new();

    let initial_pool_info = fixture.get_pool_info();
    let initial_pool_reserve_a = initial_pool_info.reserve_a;
    let initial_pool_reserve_b = initial_pool_info.reserve_b;

    // Execute flash loan
    let borrow_amount_a = 1_000_000i128;
    fixture.execute_flash_loan(borrow_amount_a, 0);

    // After repayment, pool should have its original amount plus the fee
    let final_pool_info = fixture.get_pool_info();
    let fee_bps = 5i128;
    let expected_fee = (borrow_amount_a * fee_bps) / 10_000;

    assert_eq!(
        final_pool_info.reserve_a,
        initial_pool_reserve_a + expected_fee,
        "Pool reserve should increase by exactly the fee"
    );

    assert_eq!(
        final_pool_info.reserve_b, initial_pool_reserve_b,
        "Pool reserve B should be unchanged"
    );
}

// ===========================================================================
// Failure mode tests (cited in README)
// ===========================================================================

use soroban_amm_sdk::types::SdkAmmError;

mod mock_insufficient_profit {
    use super::*;
    use soroban_sdk::{contract, contractimpl};

    #[contract]
    pub struct InsufficientProfitReceiver;

    #[contractimpl]
    impl InsufficientProfitReceiver {
        pub fn on_flash_loan(
            _env: Env,
            _token_a_amount: i128,
            _token_b_amount: i128,
            _fee_a: i128,
            _fee_b: i128,
            _data: soroban_sdk::Bytes,
        ) -> bool {
            // Strategy detects insufficient profit to cover fee and aborts cleanly
            false
        }
    }
}

#[test]
fn insufficient_profit_aborts_cleanly() {
    let fixture = FlashLoanTestEnv::new();
    let mock_receiver = fixture
        .env
        .register_contract(None, mock_insufficient_profit::InsufficientProfitReceiver);

    // Initial balances
    let pool_info_before = fixture.get_pool_info();

    // Calling flash loan on a receiver that returns false must revert with FlashLoanRepaymentFailed (18)
    let res = fixture.pool_client.try_flash_loan(
        &mock_receiver,
        &100_000,
        &0,
        &soroban_sdk::Bytes::new(&fixture.env),
    );
    assert!(res.is_err());
    let err = res.err().unwrap();
    assert_eq!(err, Ok(SdkAmmError::FlashLoanRepaymentFailed));

    // Pool balances should be completely unchanged
    let pool_info_after = fixture.get_pool_info();
    assert_eq!(pool_info_after.reserve_a, pool_info_before.reserve_a);
    assert_eq!(pool_info_after.reserve_b, pool_info_before.reserve_b);
}

mod mock_reentrant {
    use super::*;
    use soroban_sdk::{contract, contractimpl};

    #[contract]
    pub struct ReentrantReceiver;

    #[contractimpl]
    impl ReentrantReceiver {
        pub fn on_flash_loan(
            env: Env,
            _token_a_amount: i128,
            _token_b_amount: i128,
            _fee_a: i128,
            _fee_b: i128,
            _data: soroban_sdk::Bytes,
        ) -> bool {
            let pool: Address = env.storage().instance().get(&1u32).unwrap();
            let client = AmmPoolClient::new(&env, &pool);
            let _ = client.get_info();
            true
        }

        pub fn set_pool(env: Env, pool: Address) {
            env.storage().instance().set(&1u32, &pool);
        }
    }
}

#[test]
fn reentrancy_attempt_rejected() {
    let fixture = FlashLoanTestEnv::new();
    let mock_receiver = fixture
        .env
        .register_contract(None, mock_reentrant::ReentrantReceiver);
    let mock_client = mock_reentrant::ReentrantReceiverClient::new(&fixture.env, &mock_receiver);
    mock_client.set_pool(&fixture.pool);

    let res = fixture.pool_client.try_flash_loan(
        &mock_receiver,
        &100_000,
        &0,
        &soroban_sdk::Bytes::new(&fixture.env),
    );
    // Soroban's host refuses any call back into a contract that is already on
    // the call stack, so the re-entrant `get_info` is trapped by the host
    // (`InvokeError::Abort`) before the pool's own `Reentrant` guard can return
    // its contract error. Either way the whole flash loan must fail.
    let err = res.expect_err("re-entrant flash loan must fail");
    assert_eq!(err, Err(soroban_sdk::InvokeError::Abort));
}

mod mock_incomplete_repayment {
    use super::*;
    use soroban_sdk::{contract, contractimpl};

    #[contract]
    pub struct IncompleteRepaymentReceiver;

    #[contractimpl]
    impl IncompleteRepaymentReceiver {
        pub fn on_flash_loan(
            env: Env,
            token_a_amount: i128,
            _token_b_amount: i128,
            _fee_a: i128,
            _fee_b: i128,
            _data: soroban_sdk::Bytes,
        ) -> bool {
            let pool: Address = env.storage().instance().get(&1u32).unwrap();
            let token_a: Address = env.storage().instance().get(&2u32).unwrap();
            // Repay only principal, omit fee
            TokenClient::new(&env, &token_a).transfer(
                &env.current_contract_address(),
                &pool,
                &token_a_amount,
            );
            true
        }

        pub fn set_info(env: Env, pool: Address, token_a: Address) {
            env.storage().instance().set(&1u32, &pool);
            env.storage().instance().set(&2u32, &token_a);
        }
    }
}

#[test]
fn incomplete_repayment_reverts() {
    let fixture = FlashLoanTestEnv::new();
    let mock_receiver = fixture
        .env
        .register_contract(None, mock_incomplete_repayment::IncompleteRepaymentReceiver);
    let mock_client = mock_incomplete_repayment::IncompleteRepaymentReceiverClient::new(
        &fixture.env,
        &mock_receiver,
    );
    mock_client.set_info(&fixture.pool, &fixture.token_a);

    let res = fixture.pool_client.try_flash_loan(
        &mock_receiver,
        &100_000,
        &0,
        &soroban_sdk::Bytes::new(&fixture.env),
    );
    assert!(res.is_err());
    let err = res.err().unwrap();
    assert_eq!(err, Ok(SdkAmmError::FlashLoanRepaymentFailed));
}

mod mock_wrong_recipient {
    use super::*;
    use soroban_sdk::{contract, contractimpl};

    #[contract]
    pub struct WrongRecipientReceiver;

    #[contractimpl]
    impl WrongRecipientReceiver {
        pub fn on_flash_loan(
            env: Env,
            token_a_amount: i128,
            _token_b_amount: i128,
            fee_a: i128,
            _fee_b: i128,
            _data: soroban_sdk::Bytes,
        ) -> bool {
            let token_a: Address = env.storage().instance().get(&1u32).unwrap();
            let wrong_recipient = Address::generate(&env);
            TokenClient::new(&env, &token_a).transfer(
                &env.current_contract_address(),
                &wrong_recipient,
                &(token_a_amount + fee_a),
            );
            true
        }

        pub fn set_token(env: Env, token: Address) {
            env.storage().instance().set(&1u32, &token);
        }
    }
}

#[test]
fn wrong_recipient_reverts() {
    let fixture = FlashLoanTestEnv::new();
    let mock_receiver = fixture
        .env
        .register_contract(None, mock_wrong_recipient::WrongRecipientReceiver);
    let mock_client =
        mock_wrong_recipient::WrongRecipientReceiverClient::new(&fixture.env, &mock_receiver);
    mock_client.set_token(&fixture.token_a);

    // Fund receiver with fee
    StellarAssetClient::new(&fixture.env, &fixture.token_a).mint(&mock_receiver, &10_000_000_i128);

    let res = fixture.pool_client.try_flash_loan(
        &mock_receiver,
        &100_000,
        &0,
        &soroban_sdk::Bytes::new(&fixture.env),
    );
    assert!(res.is_err());
    let err = res.err().unwrap();
    assert_eq!(err, Ok(SdkAmmError::FlashLoanRepaymentFailed));
}

// ===========================================================================
// Security & Authorization tests
// ===========================================================================

#[test]
fn stranger_cannot_call_on_flash_loan() {
    let fixture = FlashLoanTestEnv::new();
    let receiver_client =
        flash_loan_receiver::FlashLoanReceiverClient::new(&fixture.env, &fixture.receiver);

    let balance_before = fixture.get_token_balance(&fixture.token_a, &fixture.receiver);

    // Clear all mocked authorizations to simulate unauthenticated stranger call
    fixture.env.mock_auths(&[]);

    // A direct call by an unauthenticated caller / stranger must fail because pool.require_auth() fails
    let res = receiver_client.try_on_flash_loan(
        &5_000_000,
        &0,
        &0,
        &0,
        &soroban_sdk::Bytes::new(&fixture.env),
    );
    assert!(
        res.is_err(),
        "Direct call by stranger must fail authorization"
    );

    // Receiver balance must be completely unchanged
    let balance_after = fixture.get_token_balance(&fixture.token_a, &fixture.receiver);
    assert_eq!(
        balance_after, balance_before,
        "Receiver balance must not change on unauthorized call"
    );
}

#[test]
fn second_initialize_fails_with_already_initialized() {
    let fixture = FlashLoanTestEnv::new();
    let receiver_client =
        flash_loan_receiver::FlashLoanReceiverClient::new(&fixture.env, &fixture.receiver);

    let attacker_pool = Address::generate(&fixture.env);
    let attacker_owner = Address::generate(&fixture.env);

    let res = receiver_client.try_initialize(&attacker_pool, &attacker_owner);
    assert!(res.is_err());
    let err = res.err().unwrap();
    assert_eq!(
        err,
        Ok(flash_loan_receiver::ReceiverError::AlreadyInitialized)
    );
}

#[test]
fn rescue_by_owner_succeeds() {
    let fixture = FlashLoanTestEnv::new();
    let receiver_client =
        flash_loan_receiver::FlashLoanReceiverClient::new(&fixture.env, &fixture.receiver);

    let recipient = Address::generate(&fixture.env);
    let rescue_amount = 1_000_000i128;

    let res = receiver_client.try_rescue(&fixture.token_a, &rescue_amount, &recipient);
    assert!(res.is_ok());

    let recipient_balance = fixture.get_token_balance(&fixture.token_a, &recipient);
    assert_eq!(recipient_balance, rescue_amount);
}

#[test]
fn rescue_by_stranger_fails() {
    let fixture = FlashLoanTestEnv::new();
    let receiver_client =
        flash_loan_receiver::FlashLoanReceiverClient::new(&fixture.env, &fixture.receiver);

    let recipient = Address::generate(&fixture.env);
    let rescue_amount = 1_000_000i128;

    // Clear mocked authorizations: stranger cannot satisfy owner.require_auth()
    fixture.env.mock_auths(&[]);

    let res = receiver_client.try_rescue(&fixture.token_a, &rescue_amount, &recipient);
    assert!(res.is_err(), "Stranger cannot invoke rescue");

    let recipient_balance = fixture.get_token_balance(&fixture.token_a, &recipient);
    assert_eq!(recipient_balance, 0);
}

#[test]
fn legitimate_flash_loan_with_mock_auths_for_borrower_succeeds() {
    let fixture = FlashLoanTestEnv::new();

    // Verify flash loan works with real pool interaction and fee deduction
    let initial_info = fixture.get_pool_info();
    let borrow_amount_a = 500_000i128;

    fixture.execute_flash_loan(borrow_amount_a, 0);

    let final_info = fixture.get_pool_info();
    let expected_fee_a = (borrow_amount_a * 5) / 10_000;
    assert_eq!(
        final_info.reserve_a,
        initial_info.reserve_a + expected_fee_a
    );
}

#[test]
fn rescue_fails_if_not_initialized() {
    let env = Env::default();
    env.mock_all_auths();
    let uninit_receiver = env.register_contract(None, FlashLoanReceiver);
    let client = flash_loan_receiver::FlashLoanReceiverClient::new(&env, &uninit_receiver);
    let token = Address::generate(&env);
    let to = Address::generate(&env);

    let res = client.try_rescue(&token, &100, &to);
    assert!(res.is_err());
    let err = res.err().unwrap();
    assert_eq!(err, Ok(flash_loan_receiver::ReceiverError::NotInitialized));
}
