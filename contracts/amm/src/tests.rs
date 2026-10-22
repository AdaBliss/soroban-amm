extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events, Ledger},
    token::{StellarAssetClient, TokenClient as StellarTokenClient},
    Env,
};
use token::LpToken;

// ── Flash-loan test helper ────────────────────────────────────────────────────

#[contracttype]
enum ReceiverDataKey {
    Amm,
    TokenA,
    TokenB,
    ShouldRepay,
    /// #698: which other guarded pool entry point (if any) to attempt from
    /// inside `on_flash_loan`, after repaying. 0 = none,
    /// 1 = `remove_liquidity_one_sided`, 2 = `swap_fot`,
    /// 3 = `withdraw_protocol_fees`.
    Attempt,
    /// #698: the `AmmError` discriminant (as `u32`) of the attempted call
    /// above, or `0` if it unexpectedly succeeded / none was attempted.
    LastCode,
}

#[contract]
pub(crate) struct MockFlashLoanReceiver;

#[contractimpl]
impl MockFlashLoanReceiver {
    pub fn initialize(
        env: Env,
        amm: Address,
        token_a: Address,
        token_b: Address,
        should_repay: bool,
    ) {
        env.storage().instance().set(&ReceiverDataKey::Amm, &amm);
        env.storage()
            .instance()
            .set(&ReceiverDataKey::TokenA, &token_a);
        env.storage()
            .instance()
            .set(&ReceiverDataKey::TokenB, &token_b);
        env.storage()
            .instance()
            .set(&ReceiverDataKey::ShouldRepay, &should_repay);
    }

    pub fn on_flash_loan(
        env: Env,
        amount_a: i128,
        amount_b: i128,
        fee_a: i128,
        fee_b: i128,
        _data: Bytes,
    ) -> bool {
        let should_repay = env
            .storage()
            .instance()
            .get(&ReceiverDataKey::ShouldRepay)
            .unwrap_or(false);
        if should_repay {
            let amm: Address = env.storage().instance().get(&ReceiverDataKey::Amm).unwrap();
            let token_a: Address = env
                .storage()
                .instance()
                .get(&ReceiverDataKey::TokenA)
                .unwrap();
            let token_b: Address = env
                .storage()
                .instance()
                .get(&ReceiverDataKey::TokenB)
                .unwrap();
            if amount_a > 0 || fee_a > 0 {
                SepTokenClient::new(&env, &token_a).transfer(
                    &env.current_contract_address(),
                    &amm,
                    &(amount_a + fee_a),
                );
            }
            if amount_b > 0 || fee_b > 0 {
                SepTokenClient::new(&env, &token_b).transfer(
                    &env.current_contract_address(),
                    &amm,
                    &(amount_b + fee_b),
                );
            }

            // #698: after repaying, optionally attempt another guarded pool
            // entry point from inside this same callback to prove it is
            // rejected while the flash-loan lock is held.
            let attempt: u32 = env
                .storage()
                .instance()
                .get(&ReceiverDataKey::Attempt)
                .unwrap_or(0);
            if attempt > 0 {
                let me = env.current_contract_address();
                let amm_client = AmmPoolClient::new(&env, &amm);
                let code: u32 = match attempt {
                    1 => match amm_client.try_remove_liquidity_one_sided(
                        &me,
                        &1_i128,
                        &token_a,
                        &0_i128,
                        &u64::MAX,
                    ) {
                        Ok(Ok(_)) => 0,
                        Ok(Err(_)) => 998,
                        Err(Ok(e)) => e as u32,
                        Err(Err(_)) => 999,
                    },
                    2 => match amm_client.try_swap_fot(
                        &me,
                        &token_a,
                        &1_i128,
                        &0_i128,
                        &0_i128,
                        &u64::MAX,
                        &None,
                    ) {
                        Ok(Ok(_)) => 0,
                        Ok(Err(_)) => 998,
                        Err(Ok(e)) => e as u32,
                        Err(Err(_)) => 999,
                    },
                    3 => match amm_client.try_withdraw_protocol_fees() {
                        Ok(Ok(_)) => 0,
                        Ok(Err(_)) => 998,
                        Err(Ok(e)) => e as u32,
                        Err(Err(_)) => 999,
                    },
                    _ => 0,
                };
                env.storage()
                    .instance()
                    .set(&ReceiverDataKey::LastCode, &code);
            }
        }
        true
    }

    /// #698: arm this receiver to attempt another guarded call from inside
    /// its next `on_flash_loan` callback. See `ReceiverDataKey::Attempt`.
    pub fn set_attempt(env: Env, attempt: u32) {
        env.storage()
            .instance()
            .set(&ReceiverDataKey::Attempt, &attempt);
    }

    /// #698: the result code recorded by the most recent attempted call.
    pub fn last_code(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&ReceiverDataKey::LastCode)
            .unwrap_or(0)
    }
}

// ── Test helpers ──────────────────────────────────────────────────────────────

/// Register a Stellar Asset Contract and return (TokenClient, StellarAssetClient).
pub(crate) fn create_sac<'a>(
    env: &'a Env,
    admin: &Address,
) -> (StellarTokenClient<'a>, StellarAssetClient<'a>) {
    let contract = env.register_stellar_asset_contract_v2(admin.clone());
    (
        StellarTokenClient::new(env, &contract.address()),
        StellarAssetClient::new(env, &contract.address()),
    )
}

pub(crate) struct TestSetup {
    pub(crate) env: Env,
    pub(crate) amm_addr: Address,
    pub(crate) lp_addr: Address,
    pub(crate) ta_addr: Address,
    pub(crate) tb_addr: Address,
    #[allow(dead_code)]
    pub(crate) admin: Address,
}

/// Minimal setup: env + uninitialized AMM + LP token. Tokens are created by
/// individual tests so each test can control the pool ratio independently.
pub(crate) fn setup() -> (Env, Address, Address, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(12345);
    let admin = Address::generate(&env);
    let amm_addr = env.register_contract(None, AmmPool);
    let lp_addr = env.register_contract(None, LpToken);

    token::LpTokenClient::new(&env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(&env, "AMM LP Token"),
        &soroban_sdk::String::from_str(&env, "ALP"),
        &7u32,
    );
    (env, admin.clone(), amm_addr, lp_addr, admin)
}

pub(crate) fn setup_pool(fee_bps: i128) -> TestSetup {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(12345);
    let admin = Address::generate(&env);
    let amm_addr = env.register_contract(None, AmmPool);
    let lp_addr = env.register_contract(None, LpToken);

    token::LpTokenClient::new(&env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(&env, "AMM LP Token"),
        &soroban_sdk::String::from_str(&env, "ALP"),
        &7u32,
    );

    let (ta, ta_sac) = create_sac(&env, &admin);
    let (tb, tb_sac) = create_sac(&env, &admin);

    AmmPoolClient::new(&env, &amm_addr).initialize(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &fee_bps,
        &admin,
        &0_i128,
    );

    let ta_addr = ta.address.clone();
    let tb_addr = tb.address.clone();
    drop((ta, ta_sac, tb, tb_sac));

    TestSetup {
        env,
        amm_addr,
        lp_addr,
        ta_addr,
        tb_addr,
        admin,
    }
}

pub(crate) fn assert_reserves_match_balances(amm: &AmmPoolClient, env: &Env) {
    let info = amm.get_info();
    let (accrued_a, accrued_b) = amm.get_accrued_fees();
    let balance_a = StellarTokenClient::new(env, &info.token_a).balance(&amm.address);
    let balance_b = StellarTokenClient::new(env, &info.token_b).balance(&amm.address);
    assert_eq!(
        info.reserve_a + accrued_a,
        balance_a,
        "token_a balance invariant violated: reserve_a ({}) + accrued_a ({}) != balance_a ({})",
        info.reserve_a,
        accrued_a,
        balance_a
    );
    assert_eq!(
        info.reserve_b + accrued_b,
        balance_b,
        "token_b balance invariant violated: reserve_b ({}) + accrued_b ({}) != balance_b ({})",
        info.reserve_b,
        accrued_b,
        balance_b
    );
}

// ── Call builders ─────────────────────────────────────────────────────────────
//
// Each builder captures the required arguments and applies sensible defaults
// for optional parameters (min_shares=0, min_out=0, deadline=u64::MAX).
// When the AMM interface grows a new optional parameter, only the builder
// default needs updating — existing test call sites remain untouched.

pub(crate) struct AddLiquidity<'a> {
    amm: &'a AmmPoolClient<'a>,
    provider: &'a Address,
    amount_a: i128,
    amount_b: i128,
    min_shares: i128,
    deadline: u64,
}

impl<'a> AddLiquidity<'a> {
    pub(crate) fn new(
        amm: &'a AmmPoolClient<'a>,
        provider: &'a Address,
        amount_a: i128,
        amount_b: i128,
    ) -> Self {
        Self {
            amm,
            provider,
            amount_a,
            amount_b,
            min_shares: 0,
            deadline: u64::MAX,
        }
    }

    pub(crate) fn min_shares(mut self, v: i128) -> Self {
        self.min_shares = v;
        self
    }

    pub(crate) fn execute(self) -> i128 {
        self.amm.add_liquidity(
            self.provider,
            &self.amount_a,
            &self.amount_b,
            &self.min_shares,
            &self.deadline,
        )
    }

    pub(crate) fn try_execute(
        self,
    ) -> Result<Result<i128, soroban_sdk::Error>, Result<AmmError, soroban_sdk::InvokeError>> {
        self.amm.try_add_liquidity(
            self.provider,
            &self.amount_a,
            &self.amount_b,
            &self.min_shares,
            &self.deadline,
        )
    }
}

pub(crate) struct Swap<'a> {
    amm: &'a AmmPoolClient<'a>,
    trader: &'a Address,
    token_in: &'a Address,
    amount_in: i128,
    min_out: i128,
    deadline: u64,
}

impl<'a> Swap<'a> {
    pub(crate) fn new(
        amm: &'a AmmPoolClient<'a>,
        trader: &'a Address,
        token_in: &'a Address,
        amount_in: i128,
    ) -> Self {
        Self {
            amm,
            trader,
            token_in,
            amount_in,
            min_out: 0,
            deadline: u64::MAX,
        }
    }

    pub(crate) fn min_out(mut self, v: i128) -> Self {
        self.min_out = v;
        self
    }

    pub(crate) fn execute(self) -> i128 {
        self.amm.swap(
            self.trader,
            self.token_in,
            &self.amount_in,
            &self.min_out,
            &self.deadline,
        )
    }

    pub(crate) fn try_execute(
        self,
    ) -> Result<Result<i128, soroban_sdk::Error>, Result<AmmError, soroban_sdk::InvokeError>> {
        self.amm.try_swap(
            self.trader,
            self.token_in,
            &self.amount_in,
            &self.min_out,
            &self.deadline,
        )
    }
}

pub(crate) struct SwapExactOut<'a> {
    amm: &'a AmmPoolClient<'a>,
    trader: &'a Address,
    token_out: &'a Address,
    amount_out: i128,
    max_in: i128,
    deadline: u64,
}

impl<'a> SwapExactOut<'a> {
    pub(crate) fn new(
        amm: &'a AmmPoolClient<'a>,
        trader: &'a Address,
        token_out: &'a Address,
        amount_out: i128,
        max_in: i128,
    ) -> Self {
        Self {
            amm,
            trader,
            token_out,
            amount_out,
            max_in,
            deadline: u64::MAX,
        }
    }

    pub(crate) fn execute(self) -> i128 {
        self.amm.swap_exact_out(
            self.trader,
            self.token_out,
            &self.amount_out,
            &self.max_in,
            &self.deadline,
        )
    }

    pub(crate) fn try_execute(
        self,
    ) -> Result<Result<i128, soroban_sdk::Error>, Result<AmmError, soroban_sdk::InvokeError>> {
        self.amm.try_swap_exact_out(
            self.trader,
            self.token_out,
            &self.amount_out,
            &self.max_in,
            &self.deadline,
        )
    }
}

pub(crate) struct RemoveLiquidity<'a> {
    amm: &'a AmmPoolClient<'a>,
    provider: &'a Address,
    shares: i128,
    min_a: i128,
    min_b: i128,
    deadline: u64,
}

impl<'a> RemoveLiquidity<'a> {
    pub(crate) fn new(amm: &'a AmmPoolClient<'a>, provider: &'a Address, shares: i128) -> Self {
        Self {
            amm,
            provider,
            shares,
            min_a: 0,
            min_b: 0,
            deadline: u64::MAX,
        }
    }

    pub(crate) fn min_a(mut self, v: i128) -> Self {
        self.min_a = v;
        self
    }

    pub(crate) fn execute(self) -> (i128, i128) {
        self.amm.remove_liquidity(
            self.provider,
            &self.shares,
            &self.min_a,
            &self.min_b,
            &self.deadline,
        )
    }

    pub(crate) fn try_execute(
        self,
    ) -> Result<Result<(i128, i128), soroban_sdk::Error>, Result<AmmError, soroban_sdk::InvokeError>>
    {
        self.amm.try_remove_liquidity(
            self.provider,
            &self.shares,
            &self.min_a,
            &self.min_b,
            &self.deadline,
        )
    }
}

// ── Initialization ────────────────────────────────────────────────────────────

// Issue #86: initialize() must persist admin, fee_recipient, and protocol_fee_bps.
#[test]
fn test_initialize_stores_admin() {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta, _) = create_sac(&env, &admin);
    let (tb, _) = create_sac(&env, &admin);
    let fee_recipient = Address::generate(&env);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &30_i128,
        &fee_recipient,
        &5_i128,
    );
    let (stored_recipient, stored_bps) = amm.get_protocol_fee();
    assert_eq!(stored_recipient, Some(fee_recipient));
    assert_eq!(stored_bps, 5);
}

#[test]
fn test_set_protocol_fee_works_after_initialize() {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta, _) = create_sac(&env, &admin);
    let (tb, _) = create_sac(&env, &admin);
    let fee_recipient = Address::generate(&env);
    let new_recipient = Address::generate(&env);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &30_i128,
        &fee_recipient,
        &0_i128,
    );
    amm.set_protocol_fee(&admin, &new_recipient, &10_i128);
    let (stored_recipient, stored_bps) = amm.get_protocol_fee();
    assert_eq!(stored_recipient, Some(new_recipient));
    assert_eq!(stored_bps, 10);
}

/// Regression test for M-02: setting protocol_fee_bps == fee_bps must be rejected
/// so that LPs always retain a portion of swap income.
#[test]
fn test_set_protocol_fee_rejects_full_capture() {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta, _) = create_sac(&env, &admin);
    let (tb, _) = create_sac(&env, &admin);
    let fee_recipient = Address::generate(&env);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    // fee_bps = 30
    amm.initialize(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &30_i128,
        &fee_recipient,
        &0_i128,
    );

    // protocol_fee_bps == fee_bps (30) must be rejected (M-02 fix).
    let result = amm.try_set_protocol_fee(&admin, &fee_recipient, &30_i128);
    assert!(
        result.is_err(),
        "protocol_fee_bps == fee_bps must be rejected"
    );

    // protocol_fee_bps > fee_bps must still be rejected.
    let result2 = amm.try_set_protocol_fee(&admin, &fee_recipient, &31_i128);
    assert!(
        result2.is_err(),
        "protocol_fee_bps > fee_bps must be rejected"
    );

    // A valid strict value (29) must still be accepted.
    let result3 = amm.try_set_protocol_fee(&admin, &fee_recipient, &29_i128);
    assert!(
        result3.is_ok(),
        "protocol_fee_bps < fee_bps must be accepted"
    );

    // Zero protocol fee must always be accepted regardless of fee_bps.
    let result4 = amm.try_set_protocol_fee(&admin, &fee_recipient, &0_i128);
    assert!(
        result4.is_ok(),
        "protocol_fee_bps == 0 must always be accepted"
    );
}

#[test]
fn test_add_and_swap() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &2_000_000_i128);

    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 2_000_000).execute();
    assert!(shares > 0);

    let info = amm.get_info();
    assert_eq!(info.reserve_a, 1_000_000);
    assert_eq!(info.reserve_b, 2_000_000);
    assert_eq!(info.flash_loan_fee_bps, 30);

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    let out = Swap::new(&amm, &trader, &ts.ta_addr, 100_000).execute();
    assert!(out > 0);
    assert!(out < 200_000);
}

#[test]
fn test_price_ratio() {
    let (env, admin, amm_addr, lp_addr, _) = setup();

    let (ta_client, ta_sac) = create_sac(&env, &admin);
    let (tb_client, tb_sac) = create_sac(&env, &admin);

    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
    );

    let provider = Address::generate(&env);
    ta_sac.mint(&provider, &2_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);

    AddLiquidity::new(&amm, &provider, 2_000_000, 1_000_000).execute();

    // reserve_a = 2_000_000, reserve_b = 1_000_000
    // price_a = 1_000_000 * 1_000_000 / 2_000_000 = 500_000
    // price_b = 2_000_000 * 1_000_000 / 1_000_000 = 2_000_000
    let (price_a, price_b) = amm.price_ratio();
    assert_eq!(price_a, 500_000);
    assert_eq!(price_b, 2_000_000);
}

#[test]
fn test_price_ratio_errors_on_empty_pool() {
    let (env, admin, amm_addr, lp_addr, _) = setup();

    let (ta_client, _) = create_sac(&env, &admin);
    let (tb_client, _) = create_sac(&env, &admin);

    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &ta_client.address,
        &tb_client.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
    );

    // No liquidity added — reserves are zero, should return typed error
    let result = amm.try_price_ratio();
    assert_eq!(result, Err(Ok(AmmError::EmptyPool)));
}

#[test]
fn test_remove_liquidity() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);

    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();
    let (out_a, out_b) = RemoveLiquidity::new(&amm, &provider, shares).execute();
    assert!(out_a > 0 && out_b > 0);
    // 1_000 shares are permanently locked on first deposit (MINIMUM_LIQUIDITY).
    assert_eq!(amm.get_info().total_shares, 1_000);
}

#[test]
fn test_initialize_twice_panics() {
    let ts = setup_pool(30);
    let amm = AmmPoolClient::new(&ts.env, &ts.amm_addr);
    let result = amm.try_initialize(
        &ts.admin,
        &ts.ta_addr,
        &ts.tb_addr,
        &ts.lp_addr,
        &30_i128,
        &ts.admin,
        &0_i128,
    );
    assert!(result.is_err());
}

#[test]
fn test_invalid_fee_panics() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let amm_addr = env.register_contract(None, AmmPool);
    let lp_addr = env.register_contract(None, LpToken);
    token::LpTokenClient::new(&env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(&env, "LP"),
        &soroban_sdk::String::from_str(&env, "LP"),
        &7u32,
    );
    let (ta, _) = create_sac(&env, &admin);
    let (tb, _) = create_sac(&env, &admin);
    let result = AmmPoolClient::new(&env, &amm_addr).try_initialize(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &10_001_i128,
        &admin,
        &0_i128,
    );
    assert!(result.is_err());
}

// ── Swap ──────────────────────────────────────────────────────────────────────

#[test]
fn test_swap_b_to_a() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    tb_sac.mint(&trader, &100_000_i128);
    let out = Swap::new(&amm, &trader, &ts.tb_addr, 100_000).execute();
    assert!(out > 0 && out < 100_000);

    let info = amm.get_info();
    assert_eq!(info.reserve_b, 1_100_000);
    assert_eq!(info.reserve_a, 1_000_000 - out);
}

#[test]
fn test_swap_slippage_panics() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    let result = Swap::new(&amm, &trader, &ts.ta_addr, 100_000)
        .min_out(200_000)
        .try_execute();
    assert!(result.is_err());
}

#[test]
fn test_fee_accrues_to_reserves() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    let amount_in = 100_000_i128;
    ta_sac.mint(&trader, &amount_in);
    let out = Swap::new(&amm, &trader, &ts.ta_addr, amount_in).execute();

    let info = amm.get_info();
    assert_eq!(info.reserve_a, 1_000_000 + amount_in);
    assert_eq!(info.reserve_b, 1_000_000 - out);
    // k must grow because fee stays in pool
    assert!(info.reserve_a * info.reserve_b > 1_000_000 * 1_000_000);
}

#[test]
fn test_swap_fot_applies_lp_rebate() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let admin = ts.admin.clone();
    let fee_recipient = Address::generate(env);
    amm.set_protocol_fee(&admin, &fee_recipient, &10_i128);
    amm.set_lp_rebate(&admin, &5_000_i128);

    let trader = Address::generate(env);
    let amount_in = 100_000_i128;
    ta_sac.mint(&trader, &amount_in);

    let (_, actual_received) = amm.swap_fot(
        &trader,
        &ts.ta_addr,
        &amount_in,
        &0_i128,
        &0_i128,
        &u64::MAX,
        &Option::<Address>::None,
    );

    assert_eq!(actual_received, amount_in);

    let info = amm.get_info();
    assert_eq!(info.reserve_a, 1_000_000 + amount_in - 50_i128);
    let (accrued_a, accrued_b) = amm.get_accrued_fees();
    assert_eq!(accrued_a, 50_i128);
    assert_eq!(accrued_b, 0_i128);
}

// ── Issue #98: swap_exact_out ─────────────────────────────────────────────────

#[test]
fn test_swap_exact_out_normal_path() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let want_out = 50_000_i128;
    let required_in = amm.get_amount_in(&ts.tb_addr, &want_out);

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &(required_in + 1_000));

    let spent =
        SwapExactOut::new(&amm, &trader, &ts.tb_addr, want_out, required_in + 1_000).execute();

    assert_eq!(spent, required_in);
    let info = amm.get_info();
    assert_eq!(info.reserve_b, 1_000_000 - want_out);
    assert_eq!(info.reserve_a, 1_000_000 + spent);
}

#[test]
fn test_swap_exact_out_slippage_panics() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    // max_in=1 is far too low for any real swap — must panic with slippage message
    let result = SwapExactOut::new(&amm, &trader, &ts.tb_addr, 50_000, 1).try_execute();
    assert!(result.is_err());
}

#[test]
fn test_swap_exact_out_invalid_token_panics() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let unknown = Address::generate(env);
    let trader = Address::generate(env);
    let result = SwapExactOut::new(&amm, &trader, &unknown, 1_000, i128::MAX).try_execute();
    assert!(result.is_err());
}

#[test]
fn test_swap_exact_out_paused_panics() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    amm.pause();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    let result = SwapExactOut::new(&amm, &trader, &ts.tb_addr, 10_000, i128::MAX).try_execute();
    assert!(result.is_err());
}

#[test]
fn test_swap_exact_out_round_trip_consistent_with_get_amount_in() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    let want_out = 500_000_i128;
    let quoted_in = amm.get_amount_in(&ts.tb_addr, &want_out);

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &quoted_in);
    let actual_in = SwapExactOut::new(&amm, &trader, &ts.tb_addr, want_out, quoted_in).execute();

    assert_eq!(actual_in, quoted_in);
}

// ── Liquidity ─────────────────────────────────────────────────────────────────

#[test]
fn test_add_liquidity_slippage_panics() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    let result = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000)
        .min_shares(i128::MAX)
        .try_execute();
    assert!(result.is_err());
}

#[test]
fn test_remove_liquidity_slippage_panics() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();
    let result = RemoveLiquidity::new(&amm, &provider, shares)
        .min_a(i128::MAX)
        .try_execute();
    assert!(result.is_err());
}

#[test]
fn test_lp_token_transfer_enables_remove() {
    // Verify fix: LP token is the single source of truth for share ownership.
    // Before fix, AMM had a stale internal Shares map that didn't update on transfers.
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let lp = token::LpTokenClient::new(env, &ts.lp_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let recipient = Address::generate(env);
    lp.transfer(&provider, &recipient, &shares);

    assert_eq!(amm.shares_of(&provider), 0);
    assert_eq!(amm.shares_of(&recipient), shares);

    let (out_a, out_b) = RemoveLiquidity::new(&amm, &recipient, shares).execute();
    assert!(out_a > 0 && out_b > 0);
    assert_eq!(amm.get_info().total_shares, 1_000);
}

#[test]
fn test_multiple_lps() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let lp1 = Address::generate(env);
    ta_sac.mint(&lp1, &1_000_000_i128);
    tb_sac.mint(&lp1, &1_000_000_i128);
    let shares1 = AddLiquidity::new(&amm, &lp1, 1_000_000, 1_000_000).execute();

    let lp2 = Address::generate(env);
    ta_sac.mint(&lp2, &500_000_i128);
    tb_sac.mint(&lp2, &500_000_i128);
    let shares2 = AddLiquidity::new(&amm, &lp2, 500_000, 500_000).execute();

    // shares1 is provider's shares (1_000 MINIMUM_LIQUIDITY locked separately),
    // so storage total = shares1 + shares2 + 1_000.
    assert_eq!(amm.get_info().total_shares, shares1 + shares2 + 1_000);

    RemoveLiquidity::new(&amm, &lp1, shares1).execute();
    RemoveLiquidity::new(&amm, &lp2, shares2).execute();
    assert_eq!(amm.get_info().total_shares, 1_000);
}

// ── Quotes ────────────────────────────────────────────────────────────────────

#[test]
fn test_get_amount_out_matches_swap() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let amount_in = 50_000_i128;
    let quoted = amm.get_amount_out(&ts.ta_addr, &amount_in);

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &amount_in);
    let actual = Swap::new(&amm, &trader, &ts.ta_addr, amount_in).execute();

    assert_eq!(quoted, actual);
}

#[test]
fn test_sequential_swaps_invariant() {
    let ts = setup_pool(30); // 0.30% fee
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    // 1. Initial liquidity
    let provider = Address::generate(env);
    let initial_amt = 1_000_000_i128;
    ta_sac.mint(&provider, &initial_amt);
    tb_sac.mint(&provider, &initial_amt);
    AddLiquidity::new(&amm, &provider, initial_amt, initial_amt).execute();

    let info = amm.get_info();
    let initial_k = info.reserve_a * info.reserve_b;
    let mut current_k = initial_k;

    // 2. Perform 10 alternating swaps
    let trader = Address::generate(env);
    let swap_amt = 10_000_i128;

    for i in 0..10 {
        if i % 2 == 0 {
            ta_sac.mint(&trader, &swap_amt);
            Swap::new(&amm, &trader, &ts.ta_addr, swap_amt).execute();
        } else {
            tb_sac.mint(&trader, &swap_amt);
            Swap::new(&amm, &trader, &ts.tb_addr, swap_amt).execute();
        }

        let new_info = amm.get_info();
        let new_k = new_info.reserve_a * new_info.reserve_b;

        assert!(
            new_k >= initial_k,
            "Invariant violated: new_k ({new_k}) < initial_k ({initial_k}) at swap {i}"
        );
        assert!(
            new_k >= current_k,
            "k decreased: new_k ({new_k}) < current_k ({current_k}) at swap {i}"
        );

        current_k = new_k;
    }

    // Final k should be strictly greater than initial k because of fees
    assert!(current_k > initial_k);
}

#[test]
fn test_get_amount_in_round_trip() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &2_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 2_000_000).execute();

    // Forward: how much B do we get for 100_000 A?
    let amount_in = 100_000_i128;
    let amount_out = amm.get_amount_out(&ts.ta_addr, &amount_in);
    assert!(amount_out > 0);

    // Reverse: how much A is needed to get exactly amount_out of B?
    let amount_in_reverse = amm.get_amount_in(&ts.tb_addr, &amount_out);

    // Due to integer rounding (+1 in get_amount_in), the reverse quote
    // should be >= the original input and at most 1 unit more.
    assert!(
        amount_in_reverse >= amount_in,
        "reverse quote should be >= original input"
    );
    assert!(
        amount_in_reverse <= amount_in + 1,
        "reverse quote should be at most 1 unit above original input"
    );
}

#[test]
fn test_remove_liquidity_emits_event() {
    use soroban_sdk::testutils::Events as _;
    use soroban_sdk::{symbol_short, vec, IntoVal};

    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);

    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();
    let (out_a, out_b) = RemoveLiquidity::new(&amm, &provider, shares).execute();

    // Find the rm_liq event among all published events
    let events = env.events().all();
    let rm_liq_event = events
        .iter()
        .find(|e| e.0 == amm.address && e.1 == vec![env, symbol_short!("rm_liq")].into_val(env))
        .expect("remove_liquidity event not found");
    let __ver_0: (u32, (Address, i128, i128, i128)) = rm_liq_event.2.into_val(env);
    assert_eq!(__ver_0.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
    let data: (Address, i128, i128, i128) = __ver_0.1;
    let expected = (provider.clone(), shares, out_a, out_b);
    assert_eq!(data, expected);
}

#[test]
fn test_swap_emits_token_out_in_event_payload() {
    use soroban_sdk::testutils::Events as _;
    use soroban_sdk::IntoVal;

    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    let amount_in = 100_000_i128;
    ta_sac.mint(&trader, &amount_in);
    let amount_out = Swap::new(&amm, &trader, &ts.ta_addr, amount_in).execute();

    let events = env.events().all();
    let swap_event = events
        .iter()
        .find(|e| {
            e.0 == amm.address && e.1 == (Symbol::new(env, "swap"), trader.clone()).into_val(env)
        })
        .expect("swap event not found");

    let __ver_1: (u32, (Address, i128, Address, i128)) = swap_event.2.into_val(env);
    assert_eq!(__ver_1.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
    let data: (Address, i128, Address, i128) = __ver_1.1;
    let expected = (
        ts.ta_addr.clone(),
        amount_in,
        ts.tb_addr.clone(),
        amount_out,
    );
    assert_eq!(data, expected);
}

#[test]
fn test_twap_oracle() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    // Add liquidity to set initial price (1:1)
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    // Initial state: accumulators should be 0
    let (cum_a, cum_b, last_ts) = amm.get_price_cumulative();
    assert_eq!(cum_a, 0);
    assert_eq!(cum_b, 0);
    assert!(last_ts > 0);

    // Jump 10 seconds ahead
    env.ledger().set_timestamp(last_ts + 10);

    // Swap A for B
    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    Swap::new(&amm, &trader, &ts.ta_addr, 100_000).execute();

    // Accumulators should have updated: price 1_000_000 * 10 seconds = 10_000_000
    let (new_cum_a, new_cum_b, new_ts) = amm.get_price_cumulative();
    assert_eq!(new_ts, last_ts + 10);
    assert_eq!(new_cum_a, 10_000_000);
    assert_eq!(new_cum_b, 10_000_000);

    // Jump another 5 seconds
    env.ledger().set_timestamp(new_ts + 5);

    // New spot price after swap:
    // reserve_a = 1_100_000, reserve_b = 1_000_000 - out
    // Price A = (1_000_000 - out) * 1_000_000 / 1_100_000
    let info = amm.get_info();
    let expected_price_a = info.reserve_b * 1_000_000 / info.reserve_a;
    let expected_price_b = info.reserve_a * 1_000_000 / info.reserve_b;

    // Perform another swap
    tb_sac.mint(&trader, &50_000_i128);
    Swap::new(&amm, &trader, &ts.tb_addr, 50_000).execute();

    let (final_cum_a, final_cum_b, final_ts) = amm.get_price_cumulative();
    assert_eq!(final_ts, new_ts + 5);
    assert_eq!(final_cum_a, new_cum_a + expected_price_a * 5);
    assert_eq!(final_cum_b, new_cum_b + expected_price_b * 5);
}

#[test]
fn test_twal_oracle() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let (cum, last_ts) = amm.get_liquidity_cumulative();
    assert_eq!(cum, 0);
    assert!(last_ts > 0);

    env.ledger().set_timestamp(last_ts + 10);
    let trader = Address::generate(env);
    ta_sac.mint(&trader, &10_000_i128);
    Swap::new(&amm, &trader, &ts.ta_addr, 10_000).execute();

    let (new_cum, new_ts) = amm.get_liquidity_cumulative();
    assert_eq!(new_ts, last_ts + 10);
    // sqrt(1e6 * 1e6) = 1e6, * 10s = 10_000_000
    assert_eq!(new_cum, 10_000_000);
}

// ── Edge cases: zero-reserve guard ────────────────────────────────────────────

#[test]
fn test_swap_on_empty_pool_panics() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &1_000_i128);
    let result = Swap::new(&amm, &trader, &ts.ta_addr, 1_000).try_execute();
    assert!(result.is_err());
}

// ── Edge cases: fee boundary ───────────────────────────────────────────────────

#[test]
fn test_fee_bps_zero_succeeds() {
    let ts = setup_pool(0);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    let amount_in = 100_000_i128;
    ta_sac.mint(&trader, &amount_in);
    let out = Swap::new(&amm, &trader, &ts.ta_addr, amount_in).execute();
    // fee_bps=0 → no discount; pure constant-product formula
    let expected = amount_in * 1_000_000 / (1_000_000 + amount_in);
    assert_eq!(out, expected);
}

#[test]
fn test_fee_bps_max_succeeds() {
    // fee_bps=9_999 is the valid upper bound (< 10_000); pool initializes successfully.
    let ts = setup_pool(9_999);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    let result = Swap::new(&amm, &trader, &ts.ta_addr, 100_000).try_execute();
    assert!(result.is_ok());
    assert!(result.unwrap().unwrap() > 0);
}

#[test]
fn test_get_amount_in_max_fee_does_not_panic() {
    // With fee_bps=10_000 in storage (e.g. legacy pool before migration),
    // get_amount_in returns AmmError::InvalidFeeBps instead of trapping or returning 0.
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    // Set legacy 10_000 fee directly in storage
    env.as_contract(&ts.amm_addr, || {
        env.storage().instance().set(&DataKey::FeeBps, &10_000_i128);
    });

    let result = amm.try_get_amount_in(&ts.tb_addr, &1_000);
    assert_eq!(result, Err(Ok(AmmError::InvalidFeeBps)));
}

// ── Edge cases: minimum share precision ───────────────────────────────────────

#[test]
fn test_min_shares_exact_succeeds() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    // Initial deposit: sqrt(1_000_000 * 1_000_000) = 1_000_000; 1_000 locked → 999_000 to provider.
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000)
        .min_shares(999_000)
        .execute();
    assert_eq!(shares, 999_000);
}

#[test]
fn test_min_shares_off_by_one_panics() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    // Provider receives 999_000 (1_000 locked); requesting 1_000_001 triggers the slippage guard.
    let result = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000)
        .min_shares(1_000_001)
        .try_execute();
    assert!(result.is_err());
}

// ── Issue #34: imbalanced deposit uses the minimum ratio ──────────────────────

#[test]
fn test_imbalanced_deposit_uses_minimum_ratio() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    // Seed pool: 1,000,000 A and 2,000,000 B (ratio 1:2)
    let seeder = Address::generate(env);
    ta_sac.mint(&seeder, &1_000_000_i128);
    tb_sac.mint(&seeder, &2_000_000_i128);
    AddLiquidity::new(&amm, &seeder, 1_000_000, 2_000_000).execute();

    // The contract uses storage total_shares (includes locked MINIMUM_LIQUIDITY).
    // Capture before the second deposit so the ratio reflects the seeded state.
    let storage_total = amm.get_info().total_shares;
    let shares_from_a = 500_000_i128 * storage_total / 1_000_000;
    let shares_from_b = 1_500_000_i128 * storage_total / 2_000_000;

    // Deposit 500,000 A and 1,500,000 B — B is 500,000 in excess of the 1:2 ratio
    let lp2 = Address::generate(env);
    ta_sac.mint(&lp2, &500_000_i128);
    tb_sac.mint(&lp2, &1_500_000_i128);
    let shares_minted = AddLiquidity::new(&amm, &lp2, 500_000, 1_500_000).execute();

    assert!(
        shares_from_a < shares_from_b,
        "TokenA should be the limiting ratio"
    );
    assert_eq!(
        shares_minted, shares_from_a,
        "shares minted must use the limiting (TokenA) ratio"
    );

    let info = amm.get_info();
    assert_eq!(info.reserve_a, 1_500_000);
    assert_eq!(info.reserve_b, 3_500_000);
}

// ── Issue #35: partial remove_liquidity leaves correct residual reserves ──────

#[test]
fn test_partial_remove_liquidity_leaves_correct_reserves() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    let provider_shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();
    // 1_000 MINIMUM_LIQUIDITY locked permanently; provider receives the rest.
    assert_eq!(provider_shares, 999_000);

    // storage total_shares = 1_000_000 (includes locked 1_000).
    let shares_to_remove = provider_shares / 4; // 249_750
    let (out_a, out_b) = RemoveLiquidity::new(&amm, &provider, shares_to_remove).execute();

    assert_eq!(out_a, 249_750);
    assert_eq!(out_b, 249_750);

    let info = amm.get_info();
    assert_eq!(info.reserve_a, 750_250);
    assert_eq!(info.reserve_b, 750_250);
    assert_eq!(info.total_shares, 750_250);
}

// ── Issue #36: swap output rate decreases as input size grows ─────────────────

#[test]
fn test_swap_output_rate_decreases_with_input_size() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let input_sizes = [1_000_i128, 10_000_i128, 100_000_i128, 500_000_i128];
    let mut prev_rate = i128::MAX;

    for &amount_in in input_sizes.iter() {
        let amount_out = amm.get_amount_out(&ts.ta_addr, &amount_in);
        // Scale by 1_000_000 to preserve precision when comparing rates
        let rate = amount_out * 1_000_000 / amount_in;
        assert!(
            rate < prev_rate,
            "effective rate {rate} at input {amount_in} should be strictly less than previous rate {prev_rate}"
        );
        prev_rate = rate;
    }
}

// ── Issue #37: overflow guard tests for near-maximum reserve values ────────────

#[test]
fn test_sqrt_handles_large_input() {
    // sqrt(10^18) = 10^9
    assert_eq!(
        AmmPool::sqrt(1_000_000_000_000_000_000_i128),
        1_000_000_000_i128
    );
    // sqrt(10^36) = 10^18; 10^36 < i128::MAX (~1.7e38)
    assert_eq!(
        AmmPool::sqrt(1_000_000_000_000_000_000_000_000_000_000_000_000_i128),
        1_000_000_000_000_000_000_i128,
    );
}

#[test]
fn test_large_reserves_add_liquidity_no_overflow() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    // 4e18 * 4e18 = 1.6e37 < i128::MAX (~1.7e38); sqrt = 4e18
    let large_amount = 4_000_000_000_000_000_000_i128;
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &large_amount);
    tb_sac.mint(&provider, &large_amount);
    let shares = AddLiquidity::new(&amm, &provider, large_amount, large_amount).execute();

    // 1_000 MINIMUM_LIQUIDITY locked on first deposit.
    assert_eq!(shares, large_amount - 1_000);
    let info = amm.get_info();
    assert_eq!(info.reserve_a, large_amount);
    assert_eq!(info.reserve_b, large_amount);
}

#[test]
fn test_large_reserves_swap_no_overflow() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let large_amount = 4_000_000_000_000_000_000_i128;
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &large_amount);
    tb_sac.mint(&provider, &large_amount);
    AddLiquidity::new(&amm, &provider, large_amount, large_amount).execute();

    // amount_in=10^9; numerator = 10^9*9970*4e18 ~ 4e31 < i128::MAX
    let trader = Address::generate(env);
    let amount_in = 1_000_000_000_i128;
    ta_sac.mint(&trader, &amount_in);
    let out = Swap::new(&amm, &trader, &ts.ta_addr, amount_in).execute();
    assert!(out > 0 && out < large_amount);
}

#[test]
fn test_large_reserves_price_ratio_no_overflow() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let large_amount = 4_000_000_000_000_000_000_i128;
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &large_amount);
    tb_sac.mint(&provider, &large_amount);
    AddLiquidity::new(&amm, &provider, large_amount, large_amount).execute();

    // price_ratio: reserve_b * 1_000_000 / reserve_a; 4e18 * 1e6 = 4e24 < i128::MAX
    let (price_a, price_b) = amm.price_ratio();
    assert_eq!(price_a, 1_000_000);
    assert_eq!(price_b, 1_000_000);
}

#[test]
fn test_large_reserves_get_amount_in_round_trip() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let large_amount = 4_000_000_000_000_000_000_i128; // 4e18
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &large_amount);
    tb_sac.mint(&provider, &large_amount);
    AddLiquidity::new(&amm, &provider, large_amount, large_amount).execute();

    // Forward: B for A
    let amount_in = 1_000_000_000_i128;
    let amount_out = amm.get_amount_out(&ts.ta_addr, &amount_in);
    assert!(amount_out > 0);

    // Reverse: A needed for B
    let amount_in_reverse = amm.get_amount_in(&ts.tb_addr, &amount_out);

    assert!(
        amount_in_reverse >= amount_in,
        "reverse quote should be >= original input"
    );
    assert!(
        amount_in_reverse <= amount_in + 1,
        "reverse quote should be at most 1 unit above original input"
    );
}

#[test]
#[should_panic]
fn test_get_amount_in_overflow() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let large_amount = 4_000_000_000_000_000_000_i128; // 4e18
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &large_amount);
    tb_sac.mint(&provider, &large_amount);
    AddLiquidity::new(&amm, &provider, large_amount, large_amount).execute();

    // 4e18 * 1e17 * 10000 = 4e39 > i128::MAX
    let _ = amm.get_amount_in(&ts.ta_addr, &100_000_000_000_000_000_i128);
}

// ─────────────────────────────────────────────────────────────────────────────
// Issue #815: get_amount_in error handling tests
// ─────────────────────────────────────────────────────────────────────────────

/// Test 1: get_amount_in rejects when amount_out equals reserve_out (InsufficientLiquidity).
#[test]
fn test_get_amount_in_rejects_amount_out_equals_reserve() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    // Get the reserve of token_b
    let pool_info = amm.get_info();
    let reserve_b = pool_info.reserve_b;

    // Try to get exactly reserve_b amount — should fail with InsufficientLiquidity
    let result = amm.try_get_amount_in(&ts.tb_addr, &reserve_b);
    assert!(
        result.is_err(),
        "get_amount_in must reject amount_out >= reserve_out"
    );

    if let Err(Ok(err)) = result {
        assert_eq!(err, AmmError::InsufficientLiquidity);
    }
}

/// Test 2: get_amount_in rejects unknown token_out (InvalidToken).
#[test]
fn test_get_amount_in_rejects_unknown_token() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    // Use an unknown token
    let unknown_token = Address::generate(env);

    let result = amm.try_get_amount_in(&unknown_token, &100_000_i128);
    assert!(result.is_err(), "get_amount_in must reject unknown token");

    if let Err(Ok(err)) = result {
        assert_eq!(err, AmmError::InvalidToken);
    }
}

/// Test 3: get_amount_in rejects on empty pool (EmptyPool).
#[test]
fn test_get_amount_in_rejects_empty_pool() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);

    // Do NOT add liquidity — pool is empty (zero reserves)

    let result = amm.try_get_amount_in(&ts.tb_addr, &100_000_i128);
    assert!(result.is_err(), "get_amount_in must reject empty pool");

    if let Err(Ok(err)) = result {
        assert_eq!(err, AmmError::EmptyPool);
    }
}

/// Test 4: Boundary test - get_amount_in succeeds when amount_out is one less than reserve.
#[test]
fn test_get_amount_in_succeeds_one_below_reserve() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let pool_info = amm.get_info();
    let reserve_b = pool_info.reserve_b;

    // amount_out = reserve_b - 1 should succeed
    let result = amm.try_get_amount_in(&ts.tb_addr, &(reserve_b - 1));
    assert!(
        result.is_ok(),
        "get_amount_in must succeed when amount_out < reserve_out"
    );

    if let Ok(Ok(amount_in)) = result {
        assert!(amount_in > 0, "amount_in must be positive");
    }
}

/// Test 5: swap_exact_out rejects when amount_out equals reserve (via try_swap_exact_out).
#[test]
fn test_swap_exact_out_rejects_insufficient_liquidity() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &10_000_000_i128);

    let pool_info = amm.get_info();
    let reserve_b = pool_info.reserve_b;

    // Try to get exactly reserve_b of token_b — should fail
    let result = amm.try_swap_exact_out(&trader, &ts.tb_addr, &reserve_b, &i128::MAX, &u64::MAX);
    assert!(
        result.is_err(),
        "swap_exact_out must reject when amount_out >= reserve_out"
    );

    if let Err(Ok(err)) = result {
        assert_eq!(err, AmmError::InsufficientLiquidity);
    }
}

/// Test 6: swap_exact_out success path returns same value as before refactor.
#[test]
fn test_swap_exact_out_success_unchanged() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &10_000_000_i128);

    let want_out = 500_000_i128;
    // Execute swap_exact_out and verify it returns the expected amount_in
    let actual_in = SwapExactOut::new(&amm, &trader, &ts.tb_addr, want_out, 10_000_000).execute();

    // Verify we got the input amount (no error, value unchanged)
    assert!(
        actual_in > 0,
        "swap_exact_out must return positive amount_in"
    );
}

/// Test 7: get_amount_in with high fee (fee_bps >= 10_000) returns Err(AmmError::InvalidFeeBps).
#[test]
fn test_get_amount_in_high_fee_returns_typed_error() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    // Set legacy 10_000 fee directly in storage
    env.as_contract(&ts.amm_addr, || {
        env.storage().instance().set(&DataKey::FeeBps, &10_000_i128);
    });

    // With fee_bps = 10_000, get_amount_in must return Err(AmmError::InvalidFeeBps), not Ok(0)
    let result = amm.try_get_amount_in(&ts.tb_addr, &500_i128);
    assert_eq!(
        result,
        Err(Ok(AmmError::InvalidFeeBps)),
        "high fee should return InvalidFeeBps"
    );
}

/// Test 8: Property test - get_amount_in never panics for valid inputs.
/// For any amount_out in [1, reserve_out - 1] and fee_bps in [0, 9_999],
/// get_amount_in returns a typed Err or Ok, never a host trap.
#[test]
fn test_get_amount_in_property_no_panic() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    let pool_info = amm.get_info();
    let reserve_b = pool_info.reserve_b;

    // Test multiple valid amounts in range [1, reserve_out - 1]
    for amount_out in [1_i128, 100_i128, 1000_i128, reserve_b - 1].iter() {
        // get_amount_in must return Ok or Err, never panic
        let result = amm.try_get_amount_in(&ts.tb_addr, amount_out);

        // Result should be well-formed (either Ok(Ok(...)) or Err(...))
        match result {
            Ok(inner) => {
                // Should be Ok(i128)
                assert!(inner.is_ok(), "inner result should be Ok");
            }
            Err(_) => {
                // Host-level error (e.g., invocation failure) — should not happen
                panic!("get_amount_in should not cause host error");
            }
        }
    }
}

// Issue #199: remove_liquidity_one_sided — provider receives only token_a.
#[test]
fn test_remove_liquidity_one_sided() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let ta_client = StellarTokenClient::new(env, &ts.ta_addr);
    let tb_client = StellarTokenClient::new(env, &ts.tb_addr);

    // LP1 seeds the pool so LP2's internal swap has residual reserves to trade against.
    let lp1 = Address::generate(env);
    ta_sac.mint(&lp1, &5_000_000_i128);
    tb_sac.mint(&lp1, &5_000_000_i128);
    AddLiquidity::new(&amm, &lp1, 5_000_000, 5_000_000).execute();

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let ta_before = ta_client.balance(&provider);
    let tb_before = tb_client.balance(&provider);

    // Remove one-sided: provider wants only token_a.
    // min_out = 1_000_000 ensures at least the proportional withdrawal.
    let total_out =
        amm.remove_liquidity_one_sided(&provider, &shares, &ts.ta_addr, &1_000_000_i128, &u64::MAX);

    let ta_after = ta_client.balance(&provider);
    let tb_after = tb_client.balance(&provider);

    // Provider received exactly total_out of token_a.
    assert_eq!(ta_after - ta_before, total_out);
    // Provider's token_b balance is unchanged — received no token_b.
    assert_eq!(tb_after, tb_before);
    // Total received is more than the proportional token_a alone because the
    // unwanted token_b was swapped internally for more token_a.
    assert!(total_out > 1_000_000);
    // LP shares are fully redeemed.
    assert_eq!(amm.shares_of(&provider), 0);
}

// Issue #199: min_out slippage guard is enforced in remove_liquidity_one_sided.
#[test]
fn test_remove_liquidity_one_sided_slippage_fails() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let lp1 = Address::generate(env);
    ta_sac.mint(&lp1, &5_000_000_i128);
    tb_sac.mint(&lp1, &5_000_000_i128);
    AddLiquidity::new(&amm, &lp1, 5_000_000, 5_000_000).execute();

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    // min_out set impossibly high — must fail.
    let result =
        amm.try_remove_liquidity_one_sided(&provider, &shares, &ts.ta_addr, &i128::MAX, &u64::MAX);
    assert!(result.is_err());
}

#[test]
fn test_remove_liquidity_one_sided_circuit_breaker() {
    let ts = setup_pool(30);
    let env = &ts.env;
    env.ledger().with_mut(|li| {
        li.sequence_number = 100;
        li.timestamp = 1_234;
    });
    let amm = AmmPoolClient::new(env, &ts.amm_addr);

    // Set circuit breaker threshold to 100 bps (1%) and cooldown to 600 seconds.
    amm.set_circuit_breaker_config(&100_i128, &600_u64);

    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let lp1 = Address::generate(env);
    ta_sac.mint(&lp1, &10_000_000_i128);
    tb_sac.mint(&lp1, &10_000_000_i128);
    AddLiquidity::new(&amm, &lp1, 10_000_000, 10_000_000).execute();

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &5_000_000_i128);
    tb_sac.mint(&provider, &5_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 5_000_000, 5_000_000).execute();

    // Attempting a large remove_liquidity_one_sided breaches the 1% threshold,
    // trips the circuit breaker (returning Ok(0) without burning shares), and
    // durably persists the pause and trigger timestamp (#1094).
    let out = amm.remove_liquidity_one_sided(&provider, &shares, &ts.ta_addr, &1_i128, &u64::MAX);
    assert_eq!(
        out, 0,
        "tripped one-sided removal must return 0 without executing"
    );
    assert_eq!(
        amm.shares_of(&provider),
        shares,
        "LP shares must not be burned on trip"
    );
    assert!(
        amm.is_paused(),
        "pool must be durably paused after circuit breaker trip"
    );
    let cfg = amm.get_circuit_breaker_config();
    assert!(
        cfg.tripped,
        "circuit breaker config must report tripped == true"
    );
    assert_eq!(
        cfg.triggered_at, 1_234,
        "triggered_at must match trip timestamp"
    );

    // Subsequent call must fail with Paused.
    let res =
        amm.try_remove_liquidity_one_sided(&provider, &shares, &ts.ta_addr, &1_i128, &u64::MAX);
    assert_eq!(res, Err(Ok(AmmError::Paused)));
}

#[test]
fn bench_swap_cost() {
    extern crate std;
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);

    env.budget().reset_default();
    Swap::new(&amm, &trader, &ts.ta_addr, 100_000).execute();
    std::println!("=== SWAP BUDGET ===");
    std::println!("{}", env.budget());
}

#[test]
fn bench_add_liquidity_cost() {
    extern crate std;
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    // seed once so we hit the "else" (proportional) branch on the measured call
    let seeder = Address::generate(env);
    ta_sac.mint(&seeder, &1_000_000_i128);
    tb_sac.mint(&seeder, &1_000_000_i128);
    AddLiquidity::new(&amm, &seeder, 1_000_000, 1_000_000).execute();

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &500_000_i128);
    tb_sac.mint(&provider, &500_000_i128);

    env.budget().reset_default();
    AddLiquidity::new(&amm, &provider, 500_000, 500_000).execute();
    std::println!("=== ADD_LIQ BUDGET ===");
    std::println!("{}", env.budget());
}

// ── #698: reentrancy guard on every fund-moving entry point ──────────────────
//
// `MaliciousToken` is a minimal SEP-41-shaped token whose `transfer` can be
// armed to call back into the pool mid-transfer, driving the real
// cross-contract dispatch path (not a mock) the same way a hostile
// permissionless-pool token would. `MockFlashLoanReceiver`'s `attempt` mode
// (below) does the same from inside a flash-loan callback.
//
// IMPORTANT FINDING, recorded here so it isn't lost: on this soroban-sdk /
// soroban-env-host version, a contract can *never* be reentered by itself —
// `Host::call_n_internal` always passes `ContractReentryMode::Prohibited`
// for every ordinary and `try_` cross-contract call
// (soroban-env-host-21.2.1/src/host.rs and host/frame.rs), so the moment
// this pool's contract ID appears a second time anywhere in the active call
// stack, the host itself rejects the call before this contract's code —
// old or new — ever runs. That is true on `main` exactly as much as on this
// branch, which the tests below confirm empirically: every one of them
// records rejection code 999 (a host-level invocation error), never 14
// (`AmmError::Reentrant`), because the reentrant call traps at the host
// boundary and this contract's own guard is never reached.
//
// These tests are kept as documentation of that platform guarantee and as
// defense-in-depth coverage (the assertion is "rejected", not "rejected
// with this specific code"), not as the regression tests that motivate this
// PR — they would not distinguish `main` from this branch, since the host
// protects both equally. The tests that do distinguish the two are the
// "lock released on every error path" group below: on `main`,
// `flash_loan`'s lock is engaged by `enter_lock` but never released on any
// of its `Err` return paths (only the final `Ok` path called `exit_lock`),
// so a single failed flash loan permanently strands the lock and bricks
// every other guarded entry point — a real, serious bug independent of the
// reentrancy question above.

#[contracttype]
enum MalTokenKey {
    Balance(Address),
    Target,
    Armed,
    LastReentryCode,
}

/// A token whose `transfer` can be armed to attempt a reentrant `swap` call
/// into a configured pool address. One-shot per armed call: the first
/// transfer after `set_armed(true)` disarms itself before attempting the
/// reentrant call, so the (rejected) reentrant call's own transfers can't
/// recurse.
#[contract]
pub(crate) struct MaliciousToken;

#[contractimpl]
impl MaliciousToken {
    // Named `mal_init` rather than `initialize` to avoid colliding with
    // `MockFlashLoanReceiver::initialize`'s contractimpl-generated helper
    // items, which share this file's module namespace.
    pub fn mal_init(env: Env, target: Address) {
        env.storage().instance().set(&MalTokenKey::Target, &target);
        env.storage().instance().set(&MalTokenKey::Armed, &false);
    }

    pub fn set_armed(env: Env, armed: bool) {
        env.storage().instance().set(&MalTokenKey::Armed, &armed);
    }

    pub fn mint(env: Env, to: Address, amount: i128) {
        let bal = Self::balance(env.clone(), to.clone());
        env.storage()
            .instance()
            .set(&MalTokenKey::Balance(to), &(bal + amount));
    }

    pub fn balance(env: Env, id: Address) -> i128 {
        env.storage()
            .instance()
            .get(&MalTokenKey::Balance(id))
            .unwrap_or(0)
    }

    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        from.require_auth();
        let from_bal = Self::balance(env.clone(), from.clone());
        let to_bal = Self::balance(env.clone(), to.clone());
        env.storage()
            .instance()
            .set(&MalTokenKey::Balance(from.clone()), &(from_bal - amount));
        env.storage()
            .instance()
            .set(&MalTokenKey::Balance(to), &(to_bal + amount));

        let armed: bool = env
            .storage()
            .instance()
            .get(&MalTokenKey::Armed)
            .unwrap_or(false);
        if armed {
            env.storage().instance().set(&MalTokenKey::Armed, &false);
            let target: Address = env.storage().instance().get(&MalTokenKey::Target).unwrap();
            let pool = AmmPoolClient::new(&env, &target);
            // The argument values only need to be well-typed: the guard
            // rejects the call before any of them are validated.
            let code: u32 = match pool.try_swap(&from, &from, &1_i128, &0_i128, &u64::MAX) {
                Ok(Ok(_)) => 0,
                Ok(Err(_)) => 998,
                Err(Ok(e)) => e as u32,
                Err(Err(_)) => 999,
            };
            env.storage()
                .instance()
                .set(&MalTokenKey::LastReentryCode, &code);
        }
    }

    pub fn last_reentry_code(env: Env) -> Option<u32> {
        env.storage().instance().get(&MalTokenKey::LastReentryCode)
    }
}

/// Deploys a pool with `MaliciousToken` as `token_a` and a normal SAC as
/// `token_b`, seeds it with real liquidity (unarmed), and returns the
/// pieces needed to drive a reentrancy test.
struct MaliciousSetup {
    env: Env,
    amm_addr: Address,
    mal_addr: Address,
    tb_addr: Address,
}

fn setup_malicious_pool() -> MaliciousSetup {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(12345);
    let admin = Address::generate(&env);
    let amm_addr = env.register_contract(None, AmmPool);
    let lp_addr = env.register_contract(None, LpToken);
    let mal_addr = env.register_contract(None, MaliciousToken);
    let (tb, tb_sac) = create_sac(&env, &admin);

    token::LpTokenClient::new(&env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(&env, "AMM LP Token"),
        &soroban_sdk::String::from_str(&env, "ALP"),
        &7u32,
    );
    MaliciousTokenClient::new(&env, &mal_addr).mal_init(&amm_addr);
    AmmPoolClient::new(&env, &amm_addr).initialize(
        &admin,
        &mal_addr,
        &tb.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
    );

    let provider = Address::generate(&env);
    MaliciousTokenClient::new(&env, &mal_addr).mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AmmPoolClient::new(&env, &amm_addr).add_liquidity(
        &provider,
        &1_000_000_i128,
        &1_000_000_i128,
        &0_i128,
        &u64::MAX,
    );

    let tb_addr = tb.address.clone();
    drop((tb, tb_sac));

    MaliciousSetup {
        env,
        amm_addr,
        mal_addr,
        tb_addr,
    }
}

#[test]
fn test_swap_blocks_reentrant_token_callback() {
    let s = setup_malicious_pool();
    let env = &s.env;
    let mal = MaliciousTokenClient::new(env, &s.mal_addr);
    let amm = AmmPoolClient::new(env, &s.amm_addr);

    let trader = Address::generate(env);
    mal.mint(&trader, &10_000_i128);

    mal.set_armed(&true);
    // This must succeed: the malicious token swallows the rejected
    // reentrant call and completes its own transfer normally.
    amm.swap(&trader, &s.mal_addr, &10_000_i128, &0_i128, &u64::MAX);

    assert_ne!(
        mal.last_reentry_code(),
        Some(0),
        "reentrant swap() called from inside token transfer must be rejected, not succeed"
    );
}

#[test]
fn test_add_liquidity_blocks_reentrant_token_callback() {
    let s = setup_malicious_pool();
    let env = &s.env;
    let mal = MaliciousTokenClient::new(env, &s.mal_addr);
    let amm = AmmPoolClient::new(env, &s.amm_addr);

    let provider = Address::generate(env);
    mal.mint(&provider, &100_000_i128);
    StellarAssetClient::new(env, &s.tb_addr).mint(&provider, &100_000_i128);

    mal.set_armed(&true);
    amm.add_liquidity(&provider, &100_000_i128, &100_000_i128, &0_i128, &u64::MAX);

    assert_ne!(
        mal.last_reentry_code(),
        Some(0),
        "reentrant swap() called from inside add_liquidity's token transfer must be rejected, not succeed"
    );
}

#[test]
fn test_remove_liquidity_blocks_reentrant_token_callback() {
    let s = setup_malicious_pool();
    let env = &s.env;
    let mal = MaliciousTokenClient::new(env, &s.mal_addr);
    let amm = AmmPoolClient::new(env, &s.amm_addr);

    let provider = Address::generate(env);
    mal.mint(&provider, &100_000_i128);
    StellarAssetClient::new(env, &s.tb_addr).mint(&provider, &100_000_i128);
    let shares = amm.add_liquidity(&provider, &100_000_i128, &100_000_i128, &0_i128, &u64::MAX);

    mal.set_armed(&true);
    // The payout transfer (pool -> provider) of the malicious token_a is
    // what triggers the callback here.
    amm.remove_liquidity(&provider, &shares, &0_i128, &0_i128, &u64::MAX);

    assert_ne!(
        mal.last_reentry_code(),
        Some(0),
        "reentrant swap() called from inside remove_liquidity's token transfer must be rejected, not succeed"
    );
}

// ── Flash-loan callback attempting other guarded entry points ────────────────
//
// Reuses `MockFlashLoanReceiver`'s `attempt` mode (see `ReceiverDataKey`
// above) rather than a second receiver contract, since two `#[contract]`
// types in this module cannot both export a method named `on_flash_loan`.

fn setup_evil_receiver(ts: &TestSetup, attempt: u32) -> Address {
    let receiver_addr = ts.env.register_contract(None, MockFlashLoanReceiver);
    let client = MockFlashLoanReceiverClient::new(&ts.env, &receiver_addr);
    client.initialize(&ts.amm_addr, &ts.ta_addr, &ts.tb_addr, &true);
    client.set_attempt(&attempt);
    receiver_addr
}

#[test]
fn test_flash_loan_callback_cannot_reenter_remove_liquidity_one_sided() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let receiver_addr = setup_evil_receiver(&ts, 1);
    ta_sac.mint(&receiver_addr, &100_i128);
    tb_sac.mint(&receiver_addr, &100_i128);

    amm.flash_loan(&receiver_addr, &1_000_i128, &1_000_i128, &Bytes::new(env));
    assert_ne!(
        MockFlashLoanReceiverClient::new(env, &receiver_addr).last_code(),
        0,
        "reentrant call from inside on_flash_loan must be rejected, not succeed"
    );
}

#[test]
fn test_flash_loan_callback_cannot_reenter_swap_fot() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let receiver_addr = setup_evil_receiver(&ts, 2);
    ta_sac.mint(&receiver_addr, &100_i128);
    tb_sac.mint(&receiver_addr, &100_i128);

    amm.flash_loan(&receiver_addr, &1_000_i128, &1_000_i128, &Bytes::new(env));
    assert_ne!(
        MockFlashLoanReceiverClient::new(env, &receiver_addr).last_code(),
        0,
        "reentrant call from inside on_flash_loan must be rejected, not succeed"
    );
}

#[test]
fn test_flash_loan_callback_cannot_reenter_withdraw_protocol_fees() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let receiver_addr = setup_evil_receiver(&ts, 3);
    ta_sac.mint(&receiver_addr, &100_i128);
    tb_sac.mint(&receiver_addr, &100_i128);

    amm.flash_loan(&receiver_addr, &1_000_i128, &1_000_i128, &Bytes::new(env));
    assert_ne!(
        MockFlashLoanReceiverClient::new(env, &receiver_addr).last_code(),
        0,
        "reentrant call from inside on_flash_loan must be rejected, not succeed"
    );
}

// ── Lock is released on every error return path ──────────────────────────────
//
// Each of these forces a distinct `Err` return from a guarded function and
// asserts `is_locked()` is false afterward — the acceptance criterion that
// matters most, since a stranded lock bricks the pool permanently.

#[test]
fn test_lock_released_after_swap_slippage_error() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &1_000_i128);
    assert!(Swap::new(&amm, &trader, &ts.ta_addr, 1_000)
        .min_out(i128::MAX)
        .try_execute()
        .is_err());
    assert!(!amm.is_locked());
}

#[test]
fn test_lock_released_after_add_liquidity_zero_amount_error() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let provider = Address::generate(env);
    assert!(amm
        .try_add_liquidity(&provider, &0_i128, &0_i128, &0_i128, &u64::MAX)
        .is_err());
    assert!(!amm.is_locked());
}

#[test]
fn test_lock_released_after_remove_liquidity_insufficient_shares_error() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    assert!(RemoveLiquidity::new(&amm, &provider, i128::MAX)
        .try_execute()
        .is_err());
    assert!(!amm.is_locked());
}

#[test]
fn test_lock_released_after_remove_liquidity_one_sided_wrong_token_error() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let wrong_token = Address::generate(env);
    assert!(amm
        .try_remove_liquidity_one_sided(&provider, &shares, &wrong_token, &0_i128, &u64::MAX)
        .is_err());
    assert!(!amm.is_locked());
}

#[test]
fn test_lock_released_after_swap_exact_out_paused_error() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    amm.pause();
    let trader = Address::generate(env);
    assert!(
        SwapExactOut::new(&amm, &trader, &ts.tb_addr, 100, i128::MAX)
            .try_execute()
            .is_err()
    );
    assert!(!amm.is_locked());
}

#[test]
fn test_lock_released_after_swap_fot_invalid_token_error() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    let wrong_token = Address::generate(env);
    assert!(amm
        .try_swap_fot(
            &trader,
            &wrong_token,
            &1_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
            &None
        )
        .is_err());
    assert!(!amm.is_locked());
}

#[test]
fn test_lock_released_after_add_liquidity_fot_zero_amount_error() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let provider = Address::generate(env);
    assert!(amm
        .try_add_liquidity_fot(
            &provider,
            &0_i128,
            &0_i128,
            &0_i128,
            &0_i128,
            &0_i128,
            &u64::MAX
        )
        .is_err());
    assert!(!amm.is_locked());
}

#[test]
fn test_lock_released_after_flash_loan_repayment_failed_error() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let receiver_addr = env.register_contract(None, MockFlashLoanReceiver);
    MockFlashLoanReceiverClient::new(env, &receiver_addr).initialize(
        &ts.amm_addr,
        &ts.ta_addr,
        &ts.tb_addr,
        &false, // should_repay = false
    );
    assert!(amm
        .try_flash_loan(&receiver_addr, &1_000_i128, &1_000_i128, &Bytes::new(env))
        .is_err());
    assert!(!amm.is_locked());
}

#[test]
fn test_lock_released_after_emergency_withdraw_unauthorized_error() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    // Configuring a multisig makes the single-admin emergency_withdraw path
    // return Unauthorized.
    amm.set_multisig(&ts.admin, &soroban_sdk::vec![env, ts.admin.clone()], &1u32);
    let to = Address::generate(env);
    assert!(amm.try_emergency_withdraw(&ts.admin, &to).is_err());
    assert!(!amm.is_locked());
}

// ── Composition is unaffected ─────────────────────────────────────────────────

#[test]
fn test_two_sequential_swaps_in_same_transaction_succeed() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &2_000_i128);
    let out1 = Swap::new(&amm, &trader, &ts.ta_addr, 1_000).execute();
    let out2 = Swap::new(&amm, &trader, &ts.ta_addr, 1_000).execute();
    assert!(out1 > 0);
    assert!(out2 > 0);
    assert!(!amm.is_locked());
}

#[test]
fn test_flash_loan_still_succeeds_when_receiver_repays() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let receiver_addr = env.register_contract(None, MockFlashLoanReceiver);
    MockFlashLoanReceiverClient::new(env, &receiver_addr).initialize(
        &ts.amm_addr,
        &ts.ta_addr,
        &ts.tb_addr,
        &true,
    );
    ta_sac.mint(&receiver_addr, &100_i128);
    tb_sac.mint(&receiver_addr, &100_i128);
    amm.flash_loan(&receiver_addr, &1_000_i128, &1_000_i128, &Bytes::new(env));
    assert!(!amm.is_locked());
}

// ── is_locked / flash_loan_locked / force_unlock ──────────────────────────────

#[test]
fn test_is_locked_and_deprecated_alias_agree() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    assert!(!amm.is_locked());
    assert!(!amm.flash_loan_locked());
}

#[test]
fn test_force_unlock_single_admin_when_no_multisig() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    // No guarded call is stranded in practice; force_unlock is exercised
    // directly to prove the authorization gate and event.
    amm.force_unlock(&soroban_sdk::vec![env]);
    assert!(!amm.is_locked());
}

#[test]
fn test_force_unlock_requires_multisig_quorum_when_configured() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let signer_1 = Address::generate(env);
    let signer_2 = Address::generate(env);
    amm.set_multisig(
        &ts.admin,
        &soroban_sdk::vec![env, signer_1.clone(), signer_2.clone()],
        &2u32,
    );

    // Only one of the two required signers: rejected.
    assert!(amm
        .try_force_unlock(&soroban_sdk::vec![env, signer_1.clone()])
        .is_err());

    // Both signers: succeeds.
    amm.force_unlock(&soroban_sdk::vec![env, signer_1, signer_2]);
    assert!(!amm.is_locked());
}

// ── Issue: multisig proposal expiry must not be refreshed by an already-approved signer ──
//
// `propose_emergency_withdraw` previously reset `MultisigProposalExpiresAt` to
// `now + MULTISIG_PROPOSAL_TTL_SECS` on *every* call, even when the calling signer
// had already approved the same recipient and no new approval was added. That let a
// single signer keep a stuck proposal (e.g. 1 of 3 required approvals) alive forever
// by periodically re-proposing, defeating `ProposalExpired` as a freshness check.
//
// The fix only refreshes the expiry when a *new* approval is recorded.

#[test]
fn test_multisig_reproposal_by_approved_signer_does_not_extend_expiry() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    // Seed the pool with reserves so an emergency withdrawal would have funds.
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let a = Address::generate(env);
    let b = Address::generate(env);
    let c = Address::generate(env);
    let recipient = Address::generate(env);
    let signers = soroban_sdk::Vec::from_array(env, [a.clone(), b.clone(), c.clone()]);
    amm.set_multisig(&ts.admin, &signers, &2u32);

    const TTL: u64 = 7 * 24 * 60 * 60; // mirrors MULTISIG_PROPOSAL_TTL_SECS
    let t0 = 100_000_u64;
    env.ledger().set_timestamp(t0);

    // Signer `a` opens the proposal. Expiry = t0 + TTL, approvals = [a].
    let pid = amm.propose_emergency_withdraw(&a, &recipient);
    assert_eq!(amm.get_multisig_proposal(&pid).unwrap().approvals.len(), 1);

    // Six days later the *already-approved* signer `a` approves again. This
    // adds no new approval and must NOT extend the expiry beyond the original
    // t0 + TTL.
    let t1 = t0 + 6 * 24 * 60 * 60;
    env.ledger().set_timestamp(t1);
    amm.approve_multisig_proposal(&a, &pid);
    assert_eq!(
        amm.get_multisig_proposal(&pid).unwrap().approvals.len(),
        1,
        "re-approving signer must not be double-counted"
    );

    // Advance just past the ORIGINAL expiry (t0 + TTL). Note t2 < t1 + TTL, so if
    // the re-propose had (incorrectly) refreshed the window, execution would instead
    // fail with InsufficientShares (1 < 2). ProposalExpired proves the window was
    // NOT extended by the already-approved signer.
    let t2 = t0 + TTL + 1;
    env.ledger().set_timestamp(t2);
    let result = amm.try_exec_multisig_emergency_wd(&b, &pid);
    assert_eq!(
        result,
        Err(Ok(AmmError::ProposalExpired)),
        "a stale proposal must lapse; an already-approved signer cannot keep it alive"
    );
}

#[test]
fn test_multisig_new_cosigner_refreshes_window_and_quorum_executes() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    // Seed the pool with reserves.
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let a = Address::generate(env);
    let b = Address::generate(env);
    let c = Address::generate(env);
    let recipient = Address::generate(env);
    let signers = soroban_sdk::Vec::from_array(env, [a.clone(), b.clone(), c.clone()]);
    amm.set_multisig(&ts.admin, &signers, &2u32);

    let token_a = StellarTokenClient::new(env, &ts.ta_addr);
    let token_b = StellarTokenClient::new(env, &ts.tb_addr);
    let pool_a_before = token_a.balance(&ts.amm_addr);
    let pool_b_before = token_b.balance(&ts.amm_addr);
    let recip_a_before = token_a.balance(&recipient);
    let recip_b_before = token_b.balance(&recipient);
    assert!(pool_a_before > 0 && pool_b_before > 0);

    const TTL: u64 = 7 * 24 * 60 * 60;
    let t0 = 100_000_u64;
    env.ledger().set_timestamp(t0);

    // Signer `a` opens the proposal: approvals = [a], expiry = t0 + TTL.
    let pid = amm.propose_emergency_withdraw(&a, &recipient);

    // A genuinely NEW signer `b` approves six days later. Reaching quorum is real
    // progress, so the window is extended once to cover the emergency delay.
    let t1 = t0 + 6 * 24 * 60 * 60;
    env.ledger().set_timestamp(t1);
    amm.approve_multisig_proposal(&b, &pid);
    assert_eq!(amm.get_multisig_proposal(&pid).unwrap().approvals.len(), 2);
    amm.pause();

    // Past the ORIGINAL expiry but inside the refreshed window, and more than
    // the 24 h delay after quorum was reached. Execution must succeed and drain
    // reserves to the recipient.
    let t2 = t0 + TTL + 1;
    assert!(t2 >= t1 + 86_400);
    env.ledger().set_timestamp(t2);
    amm.exec_multisig_emergency_wd(&a, &pid);

    assert_eq!(
        token_a.balance(&ts.amm_addr),
        0,
        "reserve_a should be drained"
    );
    assert_eq!(
        token_b.balance(&ts.amm_addr),
        0,
        "reserve_b should be drained"
    );
    assert_eq!(
        token_a.balance(&recipient),
        recip_a_before + pool_a_before,
        "recipient should receive token_a reserves"
    );
    assert_eq!(
        token_b.balance(&recipient),
        recip_b_before + pool_b_before,
        "recipient should receive token_b reserves"
    );
}

/// Guard against event-schema-versioning regressions (#921).
///
/// Walks the full event log emitted across a representative call sequence and
/// asserts that every event published by the pool carries a leading
/// `EVENT_SCHEMA_VERSION` stamp. This catches any new raw
/// `env.events().publish(...)` site that skips `emit_versioned_event!`,
/// without needing one assertion per topic.
#[test]
fn test_every_emitted_event_carries_version_stamp() {
    use soroban_sdk::testutils::Events as _;
    use soroban_sdk::{IntoVal, Val};

    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    // A sequence that exercises a broad spread of the pool's event sites,
    // including the ones migrated in #921 (`fee_upd`, `protocol_fee_set`).
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &2_000_000_i128);
    tb_sac.mint(&provider, &2_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    Swap::new(&amm, &trader, &ts.ta_addr, 100_000).execute();

    amm.update_fee(&50_i128);
    amm.set_protocol_fee(&ts.admin, &ts.admin, &10_i128);

    RemoveLiquidity::new(&amm, &provider, shares / 2).execute();

    let events = env.events().all();
    let mut pool_events = 0u32;
    for e in events.iter() {
        // Only the pool's own events are versioned; token/SAC sub-calls emit
        // their own unversioned events and are out of scope here.
        if e.0 != amm.address {
            continue;
        }
        pool_events += 1;
        // Every pool event is published as `(EVENT_SCHEMA_VERSION, payload)`.
        let decoded: (u32, Val) = e.2.into_val(env);
        assert_eq!(
            decoded.0,
            soroban_amm_sdk::EVENT_SCHEMA_VERSION,
            "event with topics {:?} is missing the schema version stamp",
            e.1
        );
    }
    assert!(
        pool_events > 0,
        "expected the call sequence to emit at least one pool event"
    );
}

// ── #928: typed NotInitialized instead of a host trap ─────────────────────────

#[test]
fn test_pre_init_calls_return_not_initialized() {
    let env = Env::default();
    env.mock_all_auths();
    let amm_addr = env.register_contract(None, AmmPool);
    let client = AmmPoolClient::new(&env, &amm_addr);
    let admin = Address::generate(&env);
    let user = Address::generate(&env);
    let token = Address::generate(&env);

    // Admin setters
    assert_eq!(
        client.try_set_oracle(&admin, &None),
        Err(Ok(AmmError::NotInitialized))
    );
    assert_eq!(
        client.try_set_max_oracle_deviation_bps(&admin, &100),
        Err(Ok(AmmError::NotInitialized))
    );
    assert_eq!(client.try_pause(), Err(Ok(AmmError::NotInitialized)));
    assert_eq!(client.try_unpause(), Err(Ok(AmmError::NotInitialized)));
    assert_eq!(
        client.try_set_protocol_fee(&admin, &user, &10),
        Err(Ok(AmmError::NotInitialized))
    );
    assert_eq!(
        client.try_set_lp_rebate(&admin, &100),
        Err(Ok(AmmError::NotInitialized))
    );
    assert_eq!(
        client.try_update_fee(&30),
        Err(Ok(AmmError::NotInitialized))
    );
    assert_eq!(
        client.try_update_flash_loan_fee(&9),
        Err(Ok(AmmError::NotInitialized))
    );
    assert_eq!(
        client.try_propose_admin(&admin, &user),
        Err(Ok(AmmError::NotInitialized))
    );

    // Liquidity and swaps
    assert_eq!(
        client.try_add_liquidity(&user, &1_000, &1_000, &0, &u64::MAX),
        Err(Ok(AmmError::NotInitialized))
    );
    assert_eq!(
        client.try_remove_liquidity(&user, &100, &0, &0, &u64::MAX),
        Err(Ok(AmmError::NotInitialized))
    );
    assert_eq!(
        client.try_swap(&user, &token, &100, &0, &u64::MAX),
        Err(Ok(AmmError::NotInitialized))
    );
    assert_eq!(
        client.try_swap_exact_out(&user, &token, &100, &1_000, &u64::MAX),
        Err(Ok(AmmError::NotInitialized))
    );
    assert_eq!(
        client.try_withdraw_protocol_fees(),
        Err(Ok(AmmError::NotInitialized))
    );

    // Quotes and views
    assert_eq!(
        client.try_get_amount_out(&token, &100),
        Err(Ok(AmmError::NotInitialized))
    );
    assert_eq!(
        client.try_get_amount_in(&token, &100),
        Err(Ok(AmmError::NotInitialized))
    );
    assert!(matches!(
        client.try_simulate_swap(&token, &100),
        Err(Ok(AmmError::NotInitialized))
    ));
    assert_eq!(client.try_get_fee_info(), Err(Ok(AmmError::NotInitialized)));
    assert!(matches!(
        client.try_get_info(),
        Err(Ok(AmmError::NotInitialized))
    ));
    assert_eq!(
        client.try_shares_of(&user),
        Err(Ok(AmmError::NotInitialized))
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Issue #1097: 100% fee get_amount_in / swap_exact_out drain protection tests
// ─────────────────────────────────────────────────────────────────────────────

/// Acceptance criterion 1: Test reproducing the free drain fails safely.
/// On a pool with fee_bps = 10_000 (legacy pool), an attacker with 0 tokens calling
/// swap_exact_out receives a typed error and no tokens move.
#[test]
fn test_1097_free_drain_reproduction_fails_safely() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    // Simulate a pool that reached 100% fee before this fix
    env.as_contract(&ts.amm_addr, || {
        env.storage().instance().set(&DataKey::FeeBps, &10_000_i128);
    });

    let attacker = Address::generate(env);
    let token_a = StellarTokenClient::new(env, &ts.ta_addr);
    let token_b = StellarTokenClient::new(env, &ts.tb_addr);

    // Attacker holds 0 token_a and 0 token_b
    assert_eq!(token_b.balance(&attacker), 0);
    assert_eq!(token_a.balance(&attacker), 0);

    // Attacker tries to drain 999_000 token_b for free
    let result =
        amm.try_swap_exact_out(&attacker, &ts.tb_addr, &999_000_i128, &i128::MAX, &u64::MAX);

    // Must return AmmError::InvalidFeeBps
    assert_eq!(
        result,
        Err(Ok(AmmError::InvalidFeeBps)),
        "swap_exact_out on 100%-fee pool must fail with InvalidFeeBps"
    );

    // No tokens move: attacker still has 0, pool still has full reserves
    assert_eq!(token_b.balance(&attacker), 0);
    assert_eq!(token_a.balance(&attacker), 0);
    let info = amm.get_info();
    assert_eq!(info.reserve_a, 1_000_000);
    assert_eq!(info.reserve_b, 1_000_000);
}

/// Acceptance criterion 2: initialize and update_fee reject 10_000 and accept 9_999.
#[test]
fn test_1097_initialize_and_update_fee_reject_10_000_and_accept_9_999() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let amm_addr = env.register_contract(None, AmmPool);
    let lp_addr = env.register_contract(None, LpToken);

    token::LpTokenClient::new(&env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(&env, "LP"),
        &soroban_sdk::String::from_str(&env, "LP"),
        &7u32,
    );

    let (ta, _) = create_sac(&env, &admin);
    let (tb, _) = create_sac(&env, &admin);
    let client = AmmPoolClient::new(&env, &amm_addr);

    // initialize rejects fee_bps = 10_000 with exact AmmError::InvalidFeeBps
    let init_10k_result = client.try_initialize(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &10_000_i128,
        &admin,
        &0_i128,
    );
    assert_eq!(init_10k_result, Err(Ok(AmmError::InvalidFeeBps)));

    // initialize accepts fee_bps = 9_999
    let init_9999_result = client.try_initialize(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &9_999_i128,
        &admin,
        &0_i128,
    );
    assert!(init_9999_result.is_ok());

    // update_fee rejects 10_000 with exact AmmError::InvalidFeeBps
    let upd_10k_result = client.try_update_fee(&10_000_i128);
    assert_eq!(upd_10k_result, Err(Ok(AmmError::InvalidFeeBps)));

    // update_fee accepts 9_999
    let upd_9999_result = client.try_update_fee(&9_999_i128);
    assert!(upd_9999_result.is_ok());
    assert_eq!(client.get_fee_info(), 9_999_i128);
}

/// Acceptance criterion 3: get_amount_in returns typed error for 100%-fee pool
/// and for zero or negative amount_out.
#[test]
fn test_1097_get_amount_in_returns_typed_error_for_100_percent_fee() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    // Legacy pool with 10_000 fee
    env.as_contract(&ts.amm_addr, || {
        env.storage().instance().set(&DataKey::FeeBps, &10_000_i128);
    });

    let result = amm.try_get_amount_in(&ts.tb_addr, &1_000_i128);
    assert_eq!(result, Err(Ok(AmmError::InvalidFeeBps)));
}

#[test]
fn test_1097_get_amount_in_returns_typed_error_for_zero_or_negative_amount_out() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    // amount_out == 0
    assert_eq!(
        amm.try_get_amount_in(&ts.tb_addr, &0_i128),
        Err(Ok(AmmError::ZeroAmount))
    );
    // amount_out < 0
    assert_eq!(
        amm.try_get_amount_in(&ts.tb_addr, &-10_i128),
        Err(Ok(AmmError::ZeroAmount))
    );
}

/// Acceptance criterion 5: swap_exact_out always transfers amount_in >= 1.
#[test]
fn test_1097_swap_exact_out_always_transfers_at_least_one() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &10_000_i128);

    // Ask for tiny output (amount_out = 1)
    let amount_in = SwapExactOut::new(&amm, &trader, &ts.tb_addr, 1, 10_000).execute();
    assert!(
        amount_in >= 1,
        "swap_exact_out must transfer amount_in >= 1"
    );
    let token_a = StellarTokenClient::new(env, &ts.ta_addr);
    let token_b = StellarTokenClient::new(env, &ts.tb_addr);
    assert_eq!(token_b.balance(&trader), 1);
    assert_eq!(token_a.balance(&trader), 10_000 - amount_in);
}

/// Verify flash loan fee accepts 10_000 (100%) and rejects > 10_000.
#[test]
fn test_1097_flash_loan_fee_10_000_is_accepted_and_safe() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let amm_addr = env.register_contract(None, AmmPool);
    let lp_addr = env.register_contract(None, LpToken);

    token::LpTokenClient::new(&env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(&env, "LP"),
        &soroban_sdk::String::from_str(&env, "LP"),
        &7u32,
    );

    let (ta, _) = create_sac(&env, &admin);
    let (tb, _) = create_sac(&env, &admin);
    let client = AmmPoolClient::new(&env, &amm_addr);

    // initialize_with_flash_loan_fee accepts flash_loan_fee_bps = 10_000
    let init_result = client.try_initialize_with_flash_loan_fee(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
        &10_000_i128,
    );
    assert!(init_result.is_ok());

    // update_flash_loan_fee accepts 10_000
    assert!(client.try_update_flash_loan_fee(&10_000_i128).is_ok());

    // update_flash_loan_fee rejects 10_001
    assert_eq!(
        client.try_update_flash_loan_fee(&10_001_i128),
        Err(Ok(AmmError::InvalidFeeBps))
    );
}

/// get_amount_out returns InvalidFeeBps when pool has fee_bps >= 10_000.
#[test]
fn test_1097_get_amount_out_rejects_100_percent_fee() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    env.as_contract(&ts.amm_addr, || {
        env.storage().instance().set(&DataKey::FeeBps, &10_000_i128);
    });

    assert_eq!(
        amm.try_get_amount_out(&ts.ta_addr, &1_000_i128),
        Err(Ok(AmmError::InvalidFeeBps))
    );
    assert_eq!(
        amm.try_simulate_swap(&ts.ta_addr, &1_000_i128),
        Err(Ok(AmmError::InvalidFeeBps))
    );
}

/// swap_exact_out rejects zero or negative amount_out.
#[test]
fn test_1097_swap_exact_out_rejects_zero_or_negative_amount_out() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    let trader = Address::generate(env);
    assert_eq!(
        amm.try_swap_exact_out(&trader, &ts.tb_addr, &0_i128, &i128::MAX, &u64::MAX),
        Err(Ok(AmmError::ZeroAmount))
    );
    assert_eq!(
        amm.try_swap_exact_out(&trader, &ts.tb_addr, &-10_i128, &i128::MAX, &u64::MAX),
        Err(Ok(AmmError::ZeroAmount))
    );
}

// ── Issue #1095: Post-drain lifecycle tests ──────────────────────────────────

#[test]
fn test_post_drain_redeposit_and_old_lp_redemption_blocked_emergency_withdraw() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let ta_client = StellarTokenClient::new(env, &ts.ta_addr);
    let tb_client = StellarTokenClient::new(env, &ts.tb_addr);

    let old_provider = Address::generate(env);
    ta_sac.mint(&old_provider, &1_000_000_000_i128);
    tb_sac.mint(&old_provider, &1_000_000_000_i128);
    let old_shares = AddLiquidity::new(&amm, &old_provider, 1_000_000_000, 1_000_000_000).execute();
    assert_eq!(old_shares, 999_999_000);

    let rescue = Address::generate(env);
    drain_via_admin(&ts, &rescue);
    assert!(amm.is_drained());
    assert_eq!(ta_client.balance(&rescue), 1_000_000_000);
    assert_eq!(tb_client.balance(&rescue), 1_000_000_000);

    // Unrelated depositor attempts to deposit 100_000 / 100_000 into the drained pool.
    let new_depositor = Address::generate(env);
    ta_sac.mint(&new_depositor, &100_000_i128);
    tb_sac.mint(&new_depositor, &100_000_i128);
    let dep_res = amm.try_add_liquidity(
        &new_depositor,
        &100_000_i128,
        &100_000_i128,
        &0_i128,
        &u64::MAX,
    );
    assert_eq!(dep_res, Err(Ok(AmmError::PoolDrained)));
    assert_eq!(ta_client.balance(&new_depositor), 100_000);
    assert_eq!(tb_client.balance(&new_depositor), 100_000);

    // Old LP holder cannot redeem anything after drain.
    let old_ta_before = ta_client.balance(&old_provider);
    let old_tb_before = tb_client.balance(&old_provider);
    let rem_res =
        amm.try_remove_liquidity(&old_provider, &100_000_i128, &0_i128, &0_i128, &u64::MAX);
    assert_eq!(rem_res, Err(Ok(AmmError::PoolDrained)));
    assert_eq!(ta_client.balance(&old_provider), old_ta_before);
    assert_eq!(tb_client.balance(&old_provider), old_tb_before);
}

#[test]
fn test_post_drain_redeposit_and_old_lp_redemption_blocked_multisig() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let ta_client = StellarTokenClient::new(env, &ts.ta_addr);
    let tb_client = StellarTokenClient::new(env, &ts.tb_addr);

    let signer1 = Address::generate(env);
    let signer2 = Address::generate(env);
    let mut signers = soroban_sdk::Vec::new(env);
    signers.push_back(signer1.clone());
    signers.push_back(signer2.clone());
    amm.set_multisig(&ts.admin, &signers, &2_u32);

    let old_provider = Address::generate(env);
    ta_sac.mint(&old_provider, &1_000_000_000_i128);
    tb_sac.mint(&old_provider, &1_000_000_000_i128);
    let old_shares = AddLiquidity::new(&amm, &old_provider, 1_000_000_000, 1_000_000_000).execute();
    assert_eq!(old_shares, 999_999_000);

    let rescue = Address::generate(env);
    drain_via_multisig(&ts, &[&signer1, &signer2], &rescue);
    assert!(amm.is_drained());

    let new_depositor = Address::generate(env);
    ta_sac.mint(&new_depositor, &100_000_i128);
    tb_sac.mint(&new_depositor, &100_000_i128);
    let dep_res = amm.try_add_liquidity(
        &new_depositor,
        &100_000_i128,
        &100_000_i128,
        &0_i128,
        &u64::MAX,
    );
    assert_eq!(dep_res, Err(Ok(AmmError::PoolDrained)));
    assert_eq!(ta_client.balance(&new_depositor), 100_000);
    assert_eq!(tb_client.balance(&new_depositor), 100_000);

    let rem_res =
        amm.try_remove_liquidity(&old_provider, &100_000_i128, &0_i128, &0_i128, &u64::MAX);
    assert_eq!(rem_res, Err(Ok(AmmError::PoolDrained)));
    assert_eq!(ta_client.balance(&old_provider), 0);
    assert_eq!(tb_client.balance(&old_provider), 0);
}

#[test]
fn test_post_drain_minimum_liquidity_shares_cannot_redeem() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    // Pool holds MINIMUM_LIQUIDITY = 1_000 LP shares.
    assert_eq!(amm.shares_of(&ts.amm_addr), 1_000);

    let rescue = Address::generate(env);
    drain_via_admin(&ts, &rescue);

    // Even if someone tries to redeem the pool's locked 1_000 shares, it fails with PoolDrained.
    let res = amm.try_remove_liquidity(&ts.amm_addr, &1_000_i128, &0_i128, &0_i128, &u64::MAX);
    assert_eq!(res, Err(Ok(AmmError::PoolDrained)));
    let res_1s = amm.try_remove_liquidity_one_sided(
        &ts.amm_addr,
        &1_000_i128,
        &ts.ta_addr,
        &0_i128,
        &u64::MAX,
    );
    assert_eq!(res_1s, Err(Ok(AmmError::PoolDrained)));
}

#[test]
fn test_drained_view_and_total_shares_preserved() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    assert!(!amm.is_drained());

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &5_000_000_i128);
    tb_sac.mint(&provider, &5_000_000_i128);
    AddLiquidity::new(&amm, &provider, 5_000_000, 5_000_000).execute();

    let shares_before = amm.get_info().total_shares;
    assert_eq!(shares_before, 5_000_000);

    let rescue = Address::generate(env);
    drain_via_admin(&ts, &rescue);

    assert!(amm.is_drained());
    assert!(amm.is_paused());
    let info_after = amm.get_info();
    assert_eq!(info_after.reserve_a, 0);
    assert_eq!(info_after.reserve_b, 0);
    assert_eq!(info_after.total_shares, shares_before);
}

#[test]
fn test_drained_state_blocks_all_liquidity_and_swap_entrypoints() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 5_000_000, 5_000_000).execute();

    let rescue = Address::generate(env);
    drain_via_admin(&ts, &rescue);

    assert_eq!(
        amm.try_add_liquidity(&provider, &1_000, &1_000, &0, &u64::MAX),
        Err(Ok(AmmError::PoolDrained))
    );
    assert_eq!(
        amm.try_add_liquidity_fot(&provider, &1_000, &1_000, &0, &0, &0, &u64::MAX),
        Err(Ok(AmmError::PoolDrained))
    );
    assert_eq!(
        amm.try_remove_liquidity(&provider, &1_000, &0, &0, &u64::MAX),
        Err(Ok(AmmError::PoolDrained))
    );
    assert_eq!(
        amm.try_remove_liquidity_one_sided(&provider, &1_000, &ts.ta_addr, &0, &u64::MAX),
        Err(Ok(AmmError::PoolDrained))
    );
    assert_eq!(
        amm.try_swap(&provider, &ts.ta_addr, &1_000, &0, &u64::MAX),
        Err(Ok(AmmError::PoolDrained))
    );
    assert_eq!(
        amm.try_swap_exact_out(&provider, &ts.tb_addr, &100, &1_000, &u64::MAX),
        Err(Ok(AmmError::PoolDrained))
    );
    assert_eq!(
        amm.try_swap_fot(&provider, &ts.ta_addr, &1_000, &0, &0, &u64::MAX, &None),
        Err(Ok(AmmError::PoolDrained))
    );
    assert_eq!(
        amm.try_flash_loan(&provider, &100, &0, &soroban_sdk::Bytes::new(env)),
        Err(Ok(AmmError::PoolDrained))
    );
}

#[test]
fn test_drained_state_blocks_unpause_and_recovery_and_second_drain() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &5_000_000_i128);
    tb_sac.mint(&provider, &5_000_000_i128);
    AddLiquidity::new(&amm, &provider, 5_000_000, 5_000_000).execute();

    let rescue = Address::generate(env);
    drain_via_admin(&ts, &rescue);

    assert_eq!(amm.try_unpause(), Err(Ok(AmmError::PoolDrained)));
    assert_eq!(
        amm.try_try_circuit_breaker_recovery(),
        Err(Ok(AmmError::PoolDrained))
    );
    // The announcement is consumed by the drain, and a new one is refused.
    assert_eq!(
        amm.try_emergency_withdraw(&ts.admin, &rescue),
        Err(Ok(AmmError::NoPendingProposal))
    );
    assert_eq!(
        amm.try_announce_emergency_withdraw(&ts.admin, &rescue),
        Err(Ok(AmmError::PoolDrained))
    );
}

#[test]
fn test_multisig_emergency_wd_below_quorum_returns_quorum_not_reached() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);

    let s1 = Address::generate(env);
    let s2 = Address::generate(env);
    let s3 = Address::generate(env);
    let mut signers = soroban_sdk::Vec::new(env);
    signers.push_back(s1.clone());
    signers.push_back(s2.clone());
    signers.push_back(s3.clone());
    amm.set_multisig(&ts.admin, &signers, &2_u32);

    let rescue = Address::generate(env);
    let pid = amm.propose_emergency_withdraw(&s1, &rescue);

    let res = amm.try_exec_multisig_emergency_wd(&s1, &pid);
    assert_eq!(res, Err(Ok(AmmError::QuorumNotReached)));
}

#[test]
fn test_multisig_emergency_wd_missing_proposal_returns_no_pending_proposal() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);

    let s1 = Address::generate(env);
    let s2 = Address::generate(env);
    let mut signers = soroban_sdk::Vec::new(env);
    signers.push_back(s1.clone());
    signers.push_back(s2.clone());
    amm.set_multisig(&ts.admin, &signers, &2_u32);

    let res = amm.try_exec_multisig_emergency_wd(&s1, &1_u64);
    assert_eq!(res, Err(Ok(AmmError::NoPendingProposal)));
}

#[test]
fn test_emergency_withdraw_clears_accrued_protocol_fees_and_blocks_fee_withdrawal() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let ta_client = StellarTokenClient::new(env, &ts.ta_addr);

    let fee_recipient = Address::generate(env);
    amm.set_protocol_fee(&ts.admin, &fee_recipient, &10_i128);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    Swap::new(&amm, &trader, &ts.ta_addr, 100_000).execute();

    let (acc_a, acc_b) = amm.get_accrued_fees();
    assert!(acc_a > 0);
    assert_eq!(acc_b, 0);

    let rescue = Address::generate(env);
    drain_via_admin(&ts, &rescue);

    // Accrued fees are cleared and the rescue address receives the full contract token balance.
    assert_eq!(amm.get_accrued_fees(), (0, 0));
    assert_eq!(ta_client.balance(&ts.amm_addr), 0);
    assert_eq!(
        amm.try_withdraw_protocol_fees(),
        Err(Ok(AmmError::PoolDrained))
    );
}

#[test]
fn test_multisig_emergency_wd_clears_accrued_protocol_fees_and_blocks_fee_withdrawal() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let ta_client = StellarTokenClient::new(env, &ts.ta_addr);

    let fee_recipient = Address::generate(env);
    amm.set_protocol_fee(&ts.admin, &fee_recipient, &10_i128);

    let s1 = Address::generate(env);
    let mut signers = soroban_sdk::Vec::new(env);
    signers.push_back(s1.clone());
    amm.set_multisig(&ts.admin, &signers, &1_u32);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    Swap::new(&amm, &trader, &ts.ta_addr, 100_000).execute();
    assert!(amm.get_accrued_fees().0 > 0);

    let rescue = Address::generate(env);
    drain_via_multisig(&ts, &[&s1], &rescue);

    assert_eq!(amm.get_accrued_fees(), (0, 0));
    assert_eq!(ta_client.balance(&ts.amm_addr), 0);
    assert_eq!(
        amm.try_withdraw_protocol_fees(),
        Err(Ok(AmmError::PoolDrained))
    );
}

// ── Issue #1094: Durable circuit breaker tests ───────────────────────────────

#[test]
fn test_circuit_breaker_swap_breach_persists_pause_timestamp_and_event() {
    let ts = setup_pool(30);
    let env = &ts.env;
    env.ledger().with_mut(|li| {
        li.sequence_number = 50;
        li.timestamp = 5_000;
    });
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let ta_client = StellarTokenClient::new(env, &ts.ta_addr);
    let tb_client = StellarTokenClient::new(env, &ts.tb_addr);

    amm.set_circuit_breaker_config(&100_i128, &600_u64);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &5_000_000_i128);

    // 5M swap on 10M/10M pool breaches 100 bps (1%) threshold immediately.
    let out = Swap::new(&amm, &trader, &ts.ta_addr, 5_000_000).execute();
    assert_eq!(out, 0, "breaching swap must return Ok(0) without executing");
    assert_eq!(
        ta_client.balance(&trader),
        5_000_000,
        "trader input must not be spent"
    );
    assert_eq!(
        tb_client.balance(&trader),
        0,
        "trader must receive 0 output"
    );

    // Verify durable state!
    assert!(amm.is_paused(), "is_paused() must be true after breach");
    let cfg = amm.get_circuit_breaker_config();
    assert_eq!(cfg.triggered_at, 5_000);
    assert!(cfg.tripped);

    // Verify circuit_break event was emitted and not rolled back.
    let events = env.events().all();
    let has_cb_event = events.iter().any(|(_, topics, _)| {
        if topics.is_empty() {
            return false;
        }
        let sym: Result<soroban_sdk::Symbol, _> =
            soroban_sdk::TryFromVal::try_from_val(env, &topics.get(0).unwrap());
        matches!(sym, Ok(s) if s == soroban_sdk::Symbol::new(env, "circuit_break"))
    });
    assert!(
        has_cb_event,
        "circuit_break event must persist in transaction events"
    );
}

#[test]
fn test_circuit_breaker_second_swap_in_same_ledger_trips_cumulatively() {
    let ts = setup_pool(30);
    let env = &ts.env;
    env.ledger().with_mut(|li| {
        li.sequence_number = 77;
        li.timestamp = 2_000;
    });
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    // Threshold 500 bps (5%).
    amm.set_circuit_breaker_config(&500_i128, &600_u64);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &1_000_000_i128);

    // First swap: 150_000 (~3% price impact < 5% threshold) -> succeeds.
    let out1 = Swap::new(&amm, &trader, &ts.ta_addr, 150_000).execute();
    assert!(out1 > 0);
    assert!(!amm.is_paused());

    // Second swap in same ledger: another 200_000 pushes cumulative deviation > 5% -> trips!
    let out2 = Swap::new(&amm, &trader, &ts.ta_addr, 200_000).execute();
    assert_eq!(out2, 0);
    assert!(amm.is_paused());
    assert!(amm.get_circuit_breaker_config().tripped);
    assert_eq!(amm.get_circuit_breaker_config().triggered_at, 2_000);
}

#[test]
fn test_circuit_breaker_while_tripped_all_mutating_entrypoints_return_paused() {
    let ts = setup_pool(30);
    let env = &ts.env;
    env.ledger().with_mut(|li| {
        li.sequence_number = 88;
        li.timestamp = 3_000;
    });
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    amm.set_circuit_breaker_config(&100_i128, &600_u64);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &20_000_000_i128);
    tb_sac.mint(&provider, &20_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    // Trip the circuit breaker.
    let out = Swap::new(&amm, &provider, &ts.ta_addr, 5_000_000).execute();
    assert_eq!(out, 0);
    assert!(amm.is_paused());

    assert_eq!(
        amm.try_swap(&provider, &ts.ta_addr, &1_000, &0, &u64::MAX),
        Err(Ok(AmmError::Paused))
    );
    assert_eq!(
        amm.try_swap_exact_out(&provider, &ts.tb_addr, &100, &10_000, &u64::MAX),
        Err(Ok(AmmError::Paused))
    );
    assert_eq!(
        amm.try_swap_fot(&provider, &ts.ta_addr, &1_000, &0, &0, &u64::MAX, &None),
        Err(Ok(AmmError::Paused))
    );
    assert_eq!(
        amm.try_add_liquidity(&provider, &1_000, &1_000, &0, &u64::MAX),
        Err(Ok(AmmError::Paused))
    );
    assert_eq!(
        amm.try_remove_liquidity(&provider, &1_000, &0, &0, &u64::MAX),
        Err(Ok(AmmError::Paused))
    );
    assert_eq!(
        amm.try_remove_liquidity_one_sided(&provider, &1_000, &ts.ta_addr, &0, &u64::MAX),
        Err(Ok(AmmError::Paused))
    );
    assert_eq!(
        amm.try_flash_loan(&provider, &100, &0, &soroban_sdk::Bytes::new(env)),
        Err(Ok(AmmError::Paused))
    );
}

#[test]
fn test_circuit_breaker_recovery_before_at_and_after_cooldown() {
    let ts = setup_pool(30);
    let env = &ts.env;
    env.ledger().with_mut(|li| {
        li.sequence_number = 90;
        li.timestamp = 1_000;
    });
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    amm.set_circuit_breaker_config(&100_i128, &600_u64);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    // Trip at timestamp = 1_000.
    let out = Swap::new(&amm, &provider, &ts.ta_addr, 5_000_000).execute();
    assert_eq!(out, 0);
    assert!(amm.is_paused());

    // 1 second before cooldown expires (1_599 < 1_000 + 600): returns false.
    env.ledger().with_mut(|li| li.timestamp = 1_599);
    assert!(!amm.try_circuit_breaker_recovery());
    assert!(amm.is_paused());
    assert!(amm.get_circuit_breaker_config().tripped);

    // Exactly at boundary (1_600 == 1_000 + 600): returns true, clears pause and triggered_at.
    env.ledger().with_mut(|li| li.timestamp = 1_600);
    assert!(amm.try_circuit_breaker_recovery());
    assert!(!amm.is_paused());
    let cfg = amm.get_circuit_breaker_config();
    assert!(!cfg.tripped);
    assert_eq!(cfg.triggered_at, 0);

    // Second call returns false.
    assert!(!amm.try_circuit_breaker_recovery());
}

#[test]
fn test_circuit_breaker_admin_unpause_clears_tripped_and_triggered_at() {
    let ts = setup_pool(30);
    let env = &ts.env;
    env.ledger().with_mut(|li| {
        li.sequence_number = 91;
        li.timestamp = 4_000;
    });
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    amm.set_circuit_breaker_config(&100_i128, &600_u64);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    let out = Swap::new(&amm, &provider, &ts.ta_addr, 5_000_000).execute();
    assert_eq!(out, 0);
    assert!(amm.get_circuit_breaker_config().tripped);

    amm.unpause();
    assert!(!amm.is_paused());
    let cfg = amm.get_circuit_breaker_config();
    assert!(!cfg.tripped);
    assert_eq!(cfg.triggered_at, 0);
}

#[test]
fn test_circuit_breaker_low_price_pool_10e13_to_10e6_trips() {
    let ts = setup_pool(30);
    let env = &ts.env;
    env.ledger().with_mut(|li| {
        li.sequence_number = 92;
        li.timestamp = 7_777;
    });
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    // 5% threshold.
    amm.set_circuit_breaker_config(&500_i128, &600_u64);

    // Reserves: 10^13 : 10^6 (price = 10^-7 < 10^-6, which truncated to 0 under 1e6 scaling).
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000_000_000, 1_000_000).execute();

    // Large swap of 2 * 10^12 token_a (~30% price move > 5% threshold) must trip!
    let trader = Address::generate(env);
    ta_sac.mint(&trader, &2_000_000_000_000_i128);
    let out = Swap::new(&amm, &trader, &ts.ta_addr, 2_000_000_000_000).execute();
    assert_eq!(
        out, 0,
        "low-price pool must not truncate baseline to 0 and must trip"
    );
    assert!(amm.is_paused());
    let cfg = amm.get_circuit_breaker_config();
    assert!(cfg.tripped);
    assert_eq!(cfg.triggered_at, 7_777);
}

#[test]
fn test_circuit_breaker_boundary_threshold_minus_one_succeeds_exact_threshold_trips() {
    // 0-fee pool for clean exact boundary calculation:
    // Reserves: 100_000_000 : 100_000_000.
    // Set threshold_bps = 100 (1.00%).
    let ts1 = setup_pool(0);
    let env1 = &ts1.env;
    env1.ledger().with_mut(|li| {
        li.sequence_number = 93;
        li.timestamp = 8_000;
    });
    let amm1 = AmmPoolClient::new(env1, &ts1.amm_addr);
    let ta_sac1 = StellarAssetClient::new(env1, &ts1.ta_addr);
    let tb_sac1 = StellarAssetClient::new(env1, &ts1.tb_addr);
    amm1.set_circuit_breaker_config(&100_i128, &600_u64);

    let lp1 = Address::generate(env1);
    ta_sac1.mint(&lp1, &100_000_000_i128);
    tb_sac1.mint(&lp1, &100_000_000_i128);
    AddLiquidity::new(&amm1, &lp1, 100_000_000, 100_000_000).execute();

    // Swap 500_000 on 100_000_000 pool:
    // post_reserve_a = 100_500_000, amount_out = 497_512, post_reserve_b = 99_502_488.
    // post_price / baseline = 99_502_488 / 100_500_000 = 0.9900745 -> deviation = 99 bps (threshold_bps - 1).
    let trader1 = Address::generate(env1);
    ta_sac1.mint(&trader1, &500_000_i128);
    let out_below = Swap::new(&amm1, &trader1, &ts1.ta_addr, 500_000).execute();
    assert!(
        out_below > 0,
        "deviation of threshold_bps - 1 (99 bps) must succeed"
    );
    assert!(!amm1.is_paused());

    // Now on a fresh identical pool, swap 503_800:
    // post_reserve_a = 100_503_800, amount_out = 501_274, post_reserve_b = 99_498_726.
    // post_price / baseline = 99_498_726 / 100_503_800 = 0.9899996 -> deviation = 100 bps (== threshold_bps).
    let ts2 = setup_pool(0);
    let env2 = &ts2.env;
    env2.ledger().with_mut(|li| {
        li.sequence_number = 93;
        li.timestamp = 8_000;
    });
    let amm2 = AmmPoolClient::new(env2, &ts2.amm_addr);
    let ta_sac2 = StellarAssetClient::new(env2, &ts2.ta_addr);
    let tb_sac2 = StellarAssetClient::new(env2, &ts2.tb_addr);
    amm2.set_circuit_breaker_config(&100_i128, &600_u64);

    let lp2 = Address::generate(env2);
    ta_sac2.mint(&lp2, &100_000_000_i128);
    tb_sac2.mint(&lp2, &100_000_000_i128);
    AddLiquidity::new(&amm2, &lp2, 100_000_000, 100_000_000).execute();

    let trader2 = Address::generate(env2);
    ta_sac2.mint(&trader2, &503_800_i128);
    let out_at = Swap::new(&amm2, &trader2, &ts2.ta_addr, 503_800).execute();
    assert_eq!(
        out_at, 0,
        "deviation of exactly threshold_bps (100 bps) must trip"
    );
    assert!(amm2.is_paused());
    assert!(amm2.get_circuit_breaker_config().tripped);
}

#[test]
fn test_circuit_breaker_swap_exact_out_breach_persists_pause() {
    let ts = setup_pool(30);
    let env = &ts.env;
    env.ledger().with_mut(|li| {
        li.sequence_number = 94;
        li.timestamp = 9_000;
    });
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    amm.set_circuit_breaker_config(&100_i128, &600_u64);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &10_000_000_i128);
    let spent = amm.swap_exact_out(
        &trader,
        &ts.tb_addr,
        &2_000_000_i128,
        &10_000_000_i128,
        &u64::MAX,
    );
    assert_eq!(spent, 0);
    assert!(amm.is_paused());
    assert!(amm.get_circuit_breaker_config().tripped);
    assert_eq!(amm.get_circuit_breaker_config().triggered_at, 9_000);
}

#[test]
fn test_circuit_breaker_swap_fot_breach_persists_pause() {
    let ts = setup_pool(30);
    let env = &ts.env;
    env.ledger().with_mut(|li| {
        li.sequence_number = 95;
        li.timestamp = 9_100;
    });
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    amm.set_circuit_breaker_config(&100_i128, &600_u64);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &5_000_000_i128);
    let (out, received) = amm.swap_fot(
        &trader,
        &ts.ta_addr,
        &5_000_000_i128,
        &0_i128,
        &0_i128,
        &u64::MAX,
        &None,
    );
    assert_eq!((out, received), (0, 0));
    assert!(amm.is_paused());
    assert!(amm.get_circuit_breaker_config().tripped);
    assert_eq!(amm.get_circuit_breaker_config().triggered_at, 9_100);
}

#[test]
fn test_circuit_breaker_baseline_resets_across_new_ledger_sequence() {
    let ts = setup_pool(30);
    let env = &ts.env;
    env.ledger().with_mut(|li| {
        li.sequence_number = 100;
        li.timestamp = 1_000;
    });
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    // 500 bps (5%) threshold.
    amm.set_circuit_breaker_config(&500_i128, &600_u64);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &1_000_000_i128);

    // Ledger 100: ~4% move -> succeeds.
    let out1 = Swap::new(&amm, &trader, &ts.ta_addr, 200_000).execute();
    assert!(out1 > 0);
    assert!(!amm.is_paused());

    // Advance to ledger 101: baseline resets to the new reserves, so another ~4% move succeeds.
    env.ledger().with_mut(|li| {
        li.sequence_number = 101;
        li.timestamp = 1_005;
    });
    let out2 = Swap::new(&amm, &trader, &ts.ta_addr, 200_000).execute();
    assert!(out2 > 0);
    assert!(!amm.is_paused());
}

#[test]
fn test_circuit_breaker_permissionless_keeper_trip() {
    let ts = setup_pool(30);
    let env = &ts.env;
    env.ledger().with_mut(|li| {
        li.sequence_number = 102;
        li.timestamp = 2_222;
    });
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    // Start with 1000 bps (10%) threshold.
    amm.set_circuit_breaker_config(&1_000_i128, &600_u64);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    // No deviation yet -> keeper returns false.
    assert!(!amm.trip_circuit_breaker());

    // Swap moves price by ~4% (< 10% threshold).
    let trader = Address::generate(env);
    ta_sac.mint(&trader, &200_000_i128);
    let out = Swap::new(&amm, &trader, &ts.ta_addr, 200_000).execute();
    assert!(out > 0);

    // Admin tightens threshold to 100 bps (1%) in same ledger; keeper trips the breaker.
    amm.set_circuit_breaker_config(&100_i128, &600_u64);
    assert!(amm.trip_circuit_breaker());
    assert!(amm.is_paused());
    assert!(amm.get_circuit_breaker_config().tripped);
    assert_eq!(amm.get_circuit_breaker_config().triggered_at, 2_222);

    // Calling keeper again while already paused returns false.
    assert!(!amm.trip_circuit_breaker());
}

#[test]
fn test_circuit_breaker_slippage_failure_does_not_trip_pool() {
    let ts = setup_pool(30);
    let env = &ts.env;
    env.ledger().with_mut(|li| {
        li.sequence_number = 103;
        li.timestamp = 3_333;
    });
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    amm.set_circuit_breaker_config(&100_i128, &600_u64);

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &10_000_000_i128);
    tb_sac.mint(&provider, &10_000_000_i128);
    AddLiquidity::new(&amm, &provider, 10_000_000, 10_000_000).execute();

    let trader = Address::generate(env);
    ta_sac.mint(&trader, &5_000_000_i128);
    let res = amm.try_swap(&trader, &ts.ta_addr, &5_000_000_i128, &i128::MAX, &u64::MAX);
    assert_eq!(res, Err(Ok(AmmError::SlippageExceeded)));
    assert!(!amm.is_paused());
}

// ── Issue #1093 Tests: remove_liquidity_one_sided reserve integrity ─────────

#[test]
fn test_remove_liquidity_one_sided_token_b_regression_1093() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let seed_lp = Address::generate(env);
    ta_sac.mint(&seed_lp, &1_000_000_i128);
    tb_sac.mint(&seed_lp, &4_000_000_i128);
    AddLiquidity::new(&amm, &seed_lp, 1_000_000, 4_000_000).execute();

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &4_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 4_000_000).execute();

    amm.remove_liquidity_one_sided(&provider, &(shares / 10), &ts.tb_addr, &0_i128, &u64::MAX);

    assert_reserves_match_balances(&amm, env);
}

#[test]
fn test_remove_liquidity_one_sided_8_ratios_exact_invariants() {
    let ratios: [(i128, i128); 4] = [
        (1_000_000, 1_000_000),
        (1_000_000, 4_000_000),
        (4_000_000, 1_000_000),
        (10_000, 10_000_000),
    ];

    for (res_a, res_b) in ratios {
        for token_side in [0u8, 1u8] {
            let ts = setup_pool(30);
            let env = &ts.env;
            let amm = AmmPoolClient::new(env, &ts.amm_addr);
            let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
            let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

            let seed_lp = Address::generate(env);
            ta_sac.mint(&seed_lp, &(res_a * 2));
            tb_sac.mint(&seed_lp, &(res_b * 2));
            AddLiquidity::new(&amm, &seed_lp, res_a * 2, res_b * 2).execute();

            let provider = Address::generate(env);
            ta_sac.mint(&provider, &res_a);
            tb_sac.mint(&provider, &res_b);
            let shares = AddLiquidity::new(&amm, &provider, res_a, res_b).execute();

            let token_out = if token_side == 0 {
                ts.ta_addr.clone()
            } else {
                ts.tb_addr.clone()
            };

            amm.remove_liquidity_one_sided(
                &provider,
                &(shares / 4),
                &token_out,
                &0_i128,
                &u64::MAX,
            );

            assert_reserves_match_balances(&amm, env);
        }
    }
}

#[test]
fn test_remove_liquidity_one_sided_parity_token_a() {
    let ts1 = setup_pool(30);
    let amm1 = AmmPoolClient::new(&ts1.env, &ts1.amm_addr);
    let ta_sac1 = StellarAssetClient::new(&ts1.env, &ts1.ta_addr);
    let tb_sac1 = StellarAssetClient::new(&ts1.env, &ts1.tb_addr);
    let ta_client1 = StellarTokenClient::new(&ts1.env, &ts1.ta_addr);

    let seed_lp1 = Address::generate(&ts1.env);
    ta_sac1.mint(&seed_lp1, &3_000_000_i128);
    tb_sac1.mint(&seed_lp1, &6_000_000_i128);
    AddLiquidity::new(&amm1, &seed_lp1, 3_000_000, 6_000_000).execute();

    let p1 = Address::generate(&ts1.env);
    ta_sac1.mint(&p1, &1_000_000_i128);
    tb_sac1.mint(&p1, &2_000_000_i128);
    let shares1 = AddLiquidity::new(&amm1, &p1, 1_000_000, 2_000_000).execute();

    let ta_before1 = ta_client1.balance(&p1);
    let received_one_sided =
        amm1.remove_liquidity_one_sided(&p1, &(shares1 / 2), &ts1.ta_addr, &0_i128, &u64::MAX);
    assert_eq!(ta_client1.balance(&p1) - ta_before1, received_one_sided);

    // Manual remove + swap on identical pool
    let ts2 = setup_pool(30);
    let amm2 = AmmPoolClient::new(&ts2.env, &ts2.amm_addr);
    let ta_sac2 = StellarAssetClient::new(&ts2.env, &ts2.ta_addr);
    let tb_sac2 = StellarAssetClient::new(&ts2.env, &ts2.tb_addr);

    let seed_lp2 = Address::generate(&ts2.env);
    ta_sac2.mint(&seed_lp2, &3_000_000_i128);
    tb_sac2.mint(&seed_lp2, &6_000_000_i128);
    AddLiquidity::new(&amm2, &seed_lp2, 3_000_000, 6_000_000).execute();

    let p2 = Address::generate(&ts2.env);
    ta_sac2.mint(&p2, &1_000_000_i128);
    tb_sac2.mint(&p2, &2_000_000_i128);
    let shares2 = AddLiquidity::new(&amm2, &p2, 1_000_000, 2_000_000).execute();

    let (withdrawn_a, withdrawn_b) =
        amm2.remove_liquidity(&p2, &(shares2 / 2), &0_i128, &0_i128, &u64::MAX);
    let swapped_a = amm2.swap(&p2, &ts2.tb_addr, &withdrawn_b, &0_i128, &u64::MAX);
    let manual_total_a = withdrawn_a + swapped_a;

    let diff = (received_one_sided - manual_total_a).abs();
    assert!(
        diff <= 1,
        "tolerance 1 stroop: one_sided={received_one_sided}, manual={manual_total_a}"
    );
    assert_reserves_match_balances(&amm1, &ts1.env);
    assert_reserves_match_balances(&amm2, &ts2.env);
}

#[test]
fn test_remove_liquidity_one_sided_parity_token_b() {
    let ts1 = setup_pool(30);
    let amm1 = AmmPoolClient::new(&ts1.env, &ts1.amm_addr);
    let ta_sac1 = StellarAssetClient::new(&ts1.env, &ts1.ta_addr);
    let tb_sac1 = StellarAssetClient::new(&ts1.env, &ts1.tb_addr);
    let tb_client1 = StellarTokenClient::new(&ts1.env, &ts1.tb_addr);

    let seed_lp1 = Address::generate(&ts1.env);
    ta_sac1.mint(&seed_lp1, &3_000_000_i128);
    tb_sac1.mint(&seed_lp1, &6_000_000_i128);
    AddLiquidity::new(&amm1, &seed_lp1, 3_000_000, 6_000_000).execute();

    let p1 = Address::generate(&ts1.env);
    ta_sac1.mint(&p1, &1_000_000_i128);
    tb_sac1.mint(&p1, &2_000_000_i128);
    let shares1 = AddLiquidity::new(&amm1, &p1, 1_000_000, 2_000_000).execute();

    let tb_before1 = tb_client1.balance(&p1);
    let received_one_sided =
        amm1.remove_liquidity_one_sided(&p1, &(shares1 / 2), &ts1.tb_addr, &0_i128, &u64::MAX);
    assert_eq!(tb_client1.balance(&p1) - tb_before1, received_one_sided);

    // Manual remove + swap on identical pool
    let ts2 = setup_pool(30);
    let amm2 = AmmPoolClient::new(&ts2.env, &ts2.amm_addr);
    let ta_sac2 = StellarAssetClient::new(&ts2.env, &ts2.ta_addr);
    let tb_sac2 = StellarAssetClient::new(&ts2.env, &ts2.tb_addr);

    let seed_lp2 = Address::generate(&ts2.env);
    ta_sac2.mint(&seed_lp2, &3_000_000_i128);
    tb_sac2.mint(&seed_lp2, &6_000_000_i128);
    AddLiquidity::new(&amm2, &seed_lp2, 3_000_000, 6_000_000).execute();

    let p2 = Address::generate(&ts2.env);
    ta_sac2.mint(&p2, &1_000_000_i128);
    tb_sac2.mint(&p2, &2_000_000_i128);
    let shares2 = AddLiquidity::new(&amm2, &p2, 1_000_000, 2_000_000).execute();

    let (withdrawn_a, withdrawn_b) =
        amm2.remove_liquidity(&p2, &(shares2 / 2), &0_i128, &0_i128, &u64::MAX);
    let swapped_b = amm2.swap(&p2, &ts2.ta_addr, &withdrawn_a, &0_i128, &u64::MAX);
    let manual_total_b = withdrawn_b + swapped_b;

    let diff = (received_one_sided - manual_total_b).abs();
    assert!(
        diff <= 1,
        "tolerance 1 stroop: one_sided={received_one_sided}, manual={manual_total_b}"
    );
    assert_reserves_match_balances(&amm1, &ts1.env);
    assert_reserves_match_balances(&amm2, &ts2.env);
}

#[test]
fn test_remove_liquidity_one_sided_protocol_fee_accrual_token_b_out() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    amm.set_protocol_fee(&ts.admin, &ts.admin, &10_i128);

    let seed_lp = Address::generate(env);
    ta_sac.mint(&seed_lp, &2_000_000_i128);
    tb_sac.mint(&seed_lp, &4_000_000_i128);
    AddLiquidity::new(&amm, &seed_lp, 2_000_000, 4_000_000).execute();

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &2_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 2_000_000).execute();

    // token_out is token_b -> unwanted token_a is swapped in. Protocol fee should accrue in token_a!
    amm.remove_liquidity_one_sided(&provider, &(shares / 2), &ts.tb_addr, &0_i128, &u64::MAX);

    let (fee_a, fee_b) = amm.get_accrued_fees();
    assert!(fee_a > 0, "protocol fee must accrue to token_a");
    assert_eq!(fee_b, 0, "no protocol fee should accrue to token_b");

    assert_reserves_match_balances(&amm, env);

    amm.withdraw_protocol_fees();
    let info = amm.get_info();
    let bal_a = StellarTokenClient::new(env, &ts.ta_addr).balance(&ts.amm_addr);
    let bal_b = StellarTokenClient::new(env, &ts.tb_addr).balance(&ts.amm_addr);
    assert_eq!(info.reserve_a, bal_a);
    assert_eq!(info.reserve_b, bal_b);
}

#[test]
fn test_remove_liquidity_one_sided_protocol_fee_accrual_token_a_out() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    amm.set_protocol_fee(&ts.admin, &ts.admin, &10_i128);

    let seed_lp = Address::generate(env);
    ta_sac.mint(&seed_lp, &2_000_000_i128);
    tb_sac.mint(&seed_lp, &4_000_000_i128);
    AddLiquidity::new(&amm, &seed_lp, 2_000_000, 4_000_000).execute();

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &2_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 2_000_000).execute();

    // token_out is token_a -> unwanted token_b is swapped in. Protocol fee should accrue in token_b!
    amm.remove_liquidity_one_sided(&provider, &(shares / 2), &ts.ta_addr, &0_i128, &u64::MAX);

    let (fee_a, fee_b) = amm.get_accrued_fees();
    assert_eq!(fee_a, 0, "no protocol fee should accrue to token_a");
    assert!(fee_b > 0, "protocol fee must accrue to token_b");

    assert_reserves_match_balances(&amm, env);

    amm.withdraw_protocol_fees();
    let info = amm.get_info();
    let bal_a = StellarTokenClient::new(env, &ts.ta_addr).balance(&ts.amm_addr);
    let bal_b = StellarTokenClient::new(env, &ts.tb_addr).balance(&ts.amm_addr);
    assert_eq!(info.reserve_a, bal_a);
    assert_eq!(info.reserve_b, bal_b);
}

#[test]
fn test_remove_liquidity_one_sided_lp_rebate() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    amm.set_protocol_fee(&ts.admin, &ts.admin, &20_i128);
    amm.set_lp_rebate(&ts.admin, &5_000_i128); // 50% rebate back to LP

    let seed_lp = Address::generate(env);
    ta_sac.mint(&seed_lp, &2_000_000_i128);
    tb_sac.mint(&seed_lp, &4_000_000_i128);
    AddLiquidity::new(&amm, &seed_lp, 2_000_000, 4_000_000).execute();

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &2_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 2_000_000).execute();

    amm.remove_liquidity_one_sided(&provider, &(shares / 2), &ts.tb_addr, &0_i128, &u64::MAX);

    assert_reserves_match_balances(&amm, env);
}

#[test]
fn test_remove_liquidity_one_sided_all_shares_redeemed() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let seed_lp = Address::generate(env);
    ta_sac.mint(&seed_lp, &5_000_000_i128);
    tb_sac.mint(&seed_lp, &5_000_000_i128);
    AddLiquidity::new(&amm, &seed_lp, 5_000_000, 5_000_000).execute();

    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    let shares = AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    amm.remove_liquidity_one_sided(&provider, &shares, &ts.tb_addr, &0_i128, &u64::MAX);

    assert_eq!(amm.shares_of(&provider), 0);
    assert_reserves_match_balances(&amm, env);
}

#[test]
fn test_remove_liquidity_one_sided_alternating_both_sides() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);

    let seed_lp = Address::generate(env);
    ta_sac.mint(&seed_lp, &10_000_000_i128);
    tb_sac.mint(&seed_lp, &20_000_000_i128);
    AddLiquidity::new(&amm, &seed_lp, 10_000_000, 20_000_000).execute();

    let p1 = Address::generate(env);
    ta_sac.mint(&p1, &2_000_000_i128);
    tb_sac.mint(&p1, &4_000_000_i128);
    let s1 = AddLiquidity::new(&amm, &p1, 2_000_000, 4_000_000).execute();

    let p2 = Address::generate(env);
    ta_sac.mint(&p2, &2_000_000_i128);
    tb_sac.mint(&p2, &4_000_000_i128);
    let s2 = AddLiquidity::new(&amm, &p2, 2_000_000, 4_000_000).execute();

    amm.remove_liquidity_one_sided(&p1, &(s1 / 2), &ts.ta_addr, &0_i128, &u64::MAX);
    assert_reserves_match_balances(&amm, env);

    amm.remove_liquidity_one_sided(&p2, &(s2 / 2), &ts.tb_addr, &0_i128, &u64::MAX);
    assert_reserves_match_balances(&amm, env);

    amm.remove_liquidity_one_sided(&p1, &(s1 / 2), &ts.tb_addr, &0_i128, &u64::MAX);
    assert_reserves_match_balances(&amm, env);
}

#[test]
fn test_remove_liquidity_one_sided_zero_fee_exact_parity() {
    let ts1 = setup_pool(0);
    let amm1 = AmmPoolClient::new(&ts1.env, &ts1.amm_addr);
    let ta_sac1 = StellarAssetClient::new(&ts1.env, &ts1.ta_addr);
    let tb_sac1 = StellarAssetClient::new(&ts1.env, &ts1.tb_addr);

    let seed1 = Address::generate(&ts1.env);
    ta_sac1.mint(&seed1, &5_000_000_i128);
    tb_sac1.mint(&seed1, &5_000_000_i128);
    AddLiquidity::new(&amm1, &seed1, 5_000_000, 5_000_000).execute();

    let p1 = Address::generate(&ts1.env);
    ta_sac1.mint(&p1, &1_000_000_i128);
    tb_sac1.mint(&p1, &1_000_000_i128);
    let s1 = AddLiquidity::new(&amm1, &p1, 1_000_000, 1_000_000).execute();

    let out1 = amm1.remove_liquidity_one_sided(&p1, &s1, &ts1.tb_addr, &0_i128, &u64::MAX);
    assert_reserves_match_balances(&amm1, &ts1.env);

    let ts2 = setup_pool(0);
    let amm2 = AmmPoolClient::new(&ts2.env, &ts2.amm_addr);
    let ta_sac2 = StellarAssetClient::new(&ts2.env, &ts2.ta_addr);
    let tb_sac2 = StellarAssetClient::new(&ts2.env, &ts2.tb_addr);

    let seed2 = Address::generate(&ts2.env);
    ta_sac2.mint(&seed2, &5_000_000_i128);
    tb_sac2.mint(&seed2, &5_000_000_i128);
    AddLiquidity::new(&amm2, &seed2, 5_000_000, 5_000_000).execute();

    let p2 = Address::generate(&ts2.env);
    ta_sac2.mint(&p2, &1_000_000_i128);
    tb_sac2.mint(&p2, &1_000_000_i128);
    let s2 = AddLiquidity::new(&amm2, &p2, 1_000_000, 1_000_000).execute();

    let (wa, wb) = amm2.remove_liquidity(&p2, &s2, &0_i128, &0_i128, &u64::MAX);
    let sw = amm2.swap(&p2, &ts2.ta_addr, &wa, &0_i128, &u64::MAX);
    assert!((out1 - (wb + sw)).abs() <= 1);
    assert_reserves_match_balances(&amm2, &ts2.env);
}

/// Test helper (#1107): an AMM from `setup()` initialised with the given fee pair.
#[allow(clippy::type_complexity)]
fn init_with_pair(
    fee_bps: i128,
    protocol_fee_bps: i128,
) -> (
    Env,
    Address,
    Address,
    Result<Result<(), soroban_sdk::ConversionError>, Result<AmmError, soroban_sdk::InvokeError>>,
) {
    let (env, admin, amm_addr, lp_addr, _) = setup();
    let (ta, _) = create_sac(&env, &admin);
    let (tb, _) = create_sac(&env, &admin);
    let fee_recipient = Address::generate(&env);
    let result = AmmPoolClient::new(&env, &amm_addr).try_initialize(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &fee_bps,
        &fee_recipient,
        &protocol_fee_bps,
    );
    (env, admin, amm_addr, result)
}

/// #1107: `initialize` rejects `protocol_fee_bps == fee_bps` (AUDIT M-03).
#[test]
fn test_initialize_rejects_protocol_fee_equal_to_fee() {
    let (_, _, _, equal) = init_with_pair(30, 30);
    assert_eq!(equal, Err(Ok(AmmError::InvalidFeeBps)));

    let (_, _, _, above) = init_with_pair(30, 31);
    assert_eq!(above, Err(Ok(AmmError::InvalidFeeBps)));

    let (_, _, _, negative) = init_with_pair(30, -1);
    assert_eq!(negative, Err(Ok(AmmError::InvalidFeeBps)));

    let (_, _, _, below) = init_with_pair(30, 29);
    assert!(
        below.is_ok(),
        "protocol_fee_bps = fee_bps - 1 must be accepted"
    );
}

/// #1107: both zero is valid; a protocol fee on a zero swap fee is not.
#[test]
fn test_initialize_zero_fee_requires_zero_protocol_fee() {
    let (_, _, _, both_zero) = init_with_pair(0, 0);
    assert!(both_zero.is_ok(), "(0, 0) must be accepted");

    let (_, _, _, protocol_only) = init_with_pair(0, 1);
    assert_eq!(protocol_only, Err(Ok(AmmError::InvalidFeeBps)));
}

/// #1107: a rejected initialize leaves the pool uninitialized.
#[test]
fn test_rejected_initialize_leaves_pool_uninitialized() {
    let (env, admin, amm_addr, rejected) = init_with_pair(30, 30);
    assert!(rejected.is_err());
    let amm = AmmPoolClient::new(&env, &amm_addr);
    assert_eq!(amm.try_get_fee_info(), Err(Ok(AmmError::NotInitialized)));

    // The same address can still be initialized with a valid pair.
    let (ta, _) = create_sac(&env, &admin);
    let (tb, _) = create_sac(&env, &admin);
    let lp = env.register_contract(None, LpToken);
    amm.initialize(
        &admin,
        &ta.address,
        &tb.address,
        &lp,
        &30_i128,
        &admin,
        &10_i128,
    );
    assert_eq!(amm.get_info().fee_bps, 30);
}

/// #1107: `update_fee` rejects fee == protocol fee (the #506 regression).
#[test]
fn test_update_fee_rejects_equal_or_below_protocol_fee() {
    let (env, _admin, amm_addr, init) = init_with_pair(30, 10);
    assert!(init.is_ok());
    let amm = AmmPoolClient::new(&env, &amm_addr);

    // Strictly above the protocol fee is valid.
    assert!(amm.try_update_fee(&11_i128).is_ok());
    assert_eq!(amm.get_info().fee_bps, 11);
    amm.update_fee(&30_i128);

    // Equal to the protocol fee is the all-fees-to-protocol state.
    assert_eq!(
        amm.try_update_fee(&10_i128),
        Err(Ok(AmmError::InvalidFeeBps))
    );
    // Below it is rejected too.
    assert_eq!(
        amm.try_update_fee(&9_i128),
        Err(Ok(AmmError::InvalidFeeBps))
    );
    // Zero swap fee with a non-zero protocol fee is rejected.
    assert_eq!(
        amm.try_update_fee(&0_i128),
        Err(Ok(AmmError::InvalidFeeBps))
    );
    // Nothing was clamped or changed by the rejected calls.
    assert_eq!(amm.get_info().fee_bps, 30);
    assert_eq!(amm.get_protocol_fee().1, 10);
}

/// #1107: with the protocol fee disabled any valid swap fee is accepted.
#[test]
fn test_update_fee_works_with_zero_protocol_fee() {
    let (env, _admin, amm_addr, init) = init_with_pair(30, 0);
    assert!(init.is_ok());
    let amm = AmmPoolClient::new(&env, &amm_addr);

    assert!(
        amm.try_update_fee(&0_i128).is_ok(),
        "(0, 0) must be accepted"
    );
    assert!(amm.try_update_fee(&50_i128).is_ok());
    assert!(amm.try_update_fee(&0_i128).is_ok());
    assert_eq!(amm.get_info().fee_bps, 0);
}

/// #1107: `update_fee(protocol_fee_bps + 1)` works and `update_fee(protocol_fee_bps)` does not.
#[test]
fn test_update_fee_boundary_around_protocol_fee() {
    let (env, _admin, amm_addr, init) = init_with_pair(100, 25);
    assert!(init.is_ok());
    let amm = AmmPoolClient::new(&env, &amm_addr);

    assert_eq!(
        amm.try_update_fee(&25_i128),
        Err(Ok(AmmError::InvalidFeeBps))
    );
    assert!(amm.try_update_fee(&26_i128).is_ok());
    assert_eq!(amm.get_info().fee_bps, 26);
}

/// #1107: lowering the fee below the protocol cut is rejected, not clamped; the
/// admin lowers the protocol fee first.
#[test]
fn test_update_fee_does_not_clamp_protocol_fee() {
    let (env, admin, amm_addr, init) = init_with_pair(100, 50);
    assert!(init.is_ok());
    let amm = AmmPoolClient::new(&env, &amm_addr);

    assert_eq!(
        amm.try_update_fee(&40_i128),
        Err(Ok(AmmError::InvalidFeeBps))
    );
    assert_eq!(
        amm.get_protocol_fee().1,
        50,
        "protocol fee must not be clamped"
    );

    let recipient = Address::generate(&env);
    amm.set_protocol_fee(&admin, &recipient, &10_i128);
    assert!(amm.try_update_fee(&40_i128).is_ok());
    assert_eq!(amm.get_info().fee_bps, 40);
}

/// #1107: the validator itself, over every small pair.
#[test]
fn test_validate_fee_pair_matches_the_invariant_for_small_values() {
    for fee in -2_i128..=12 {
        for protocol in -2_i128..=14 {
            let expected = (0..10_000).contains(&fee)
                && ((fee == 0 && protocol == 0) || (protocol >= 0 && protocol < fee));
            assert_eq!(
                AmmPool::validate_fee_pair(fee, protocol).is_ok(),
                expected,
                "fee {fee}, protocol {protocol}"
            );
        }
    }
    assert!(AmmPool::validate_fee_pair(9_999, 9_998).is_ok());
    assert!(AmmPool::validate_fee_pair(9_999, 9_999).is_err());
    assert!(AmmPool::validate_fee_pair(10_000, 0).is_err());
}

/// #1107: `initialize`, `update_fee` and `set_protocol_fee` accept exactly the
/// same set of (fee_bps, protocol_fee_bps) pairs.
#[test]
fn test_fee_entry_points_accept_exactly_the_same_pairs() {
    for fee in [0_i128, 1, 5, 30, 100, 9_999] {
        for protocol in [0_i128, 1, fee - 1, fee, fee + 1] {
            let expected = (fee == 0 && protocol == 0) || (0 <= protocol && protocol < fee);

            // initialize
            let (_, _, _, init) = init_with_pair(fee, protocol);
            let init_ok = init.is_ok();

            // set_protocol_fee: a valid pool whose swap fee is moved to `fee`
            // directly in storage, then the protocol fee is set.
            let (env, admin, amm_addr, base) = init_with_pair(9_999, 0);
            assert!(base.is_ok());
            env.as_contract(&amm_addr, || {
                env.storage().instance().set(&DataKey::FeeBps, &fee);
            });
            let recipient = Address::generate(&env);
            let set_ok = AmmPoolClient::new(&env, &amm_addr)
                .try_set_protocol_fee(&admin, &recipient, &protocol)
                .is_ok();

            // update_fee: a valid pool whose protocol fee is moved to
            // `protocol` directly in storage, then the swap fee is updated.
            let (env, _admin, amm_addr, base) = init_with_pair(9_999, 0);
            assert!(base.is_ok());
            env.as_contract(&amm_addr, || {
                env.storage()
                    .instance()
                    .set(&DataKey::ProtocolFeeBps, &protocol);
            });
            let update_ok = AmmPoolClient::new(&env, &amm_addr)
                .try_update_fee(&fee)
                .is_ok();

            assert_eq!(init_ok, expected, "initialize({fee}, {protocol})");
            assert_eq!(set_ok, expected, "set_protocol_fee({fee}, {protocol})");
            assert_eq!(update_ok, expected, "update_fee({fee}, {protocol})");
        }
    }
}

// ── Emergency withdrawal: k-of-n guard + announced delay ─────────────────────
//
// Helpers that drive the full hardened flow with the default 24 h delay.

/// Drain the pool through the single-admin path: pause, announce `to`, wait out
/// the default delay, then withdraw.
fn drain_via_admin(ts: &TestSetup, to: &Address) {
    let amm = AmmPoolClient::new(&ts.env, &ts.amm_addr);
    amm.pause();
    amm.announce_emergency_withdraw(&ts.admin, to);
    ts.env
        .ledger()
        .set_timestamp(ts.env.ledger().timestamp() + 86_400);
    amm.emergency_withdraw(&ts.admin, to);
}

/// Drain the pool through the multisig path: the first signer proposes, the
/// rest approve, then the pool is paused, the default delay passes and the
/// first signer executes.
fn drain_via_multisig(ts: &TestSetup, signers: &[&Address], to: &Address) {
    let amm = AmmPoolClient::new(&ts.env, &ts.amm_addr);
    let pid = amm.propose_emergency_withdraw(signers[0], to);
    for signer in &signers[1..] {
        amm.approve_multisig_proposal(signer, &pid);
    }
    amm.pause();
    ts.env
        .ledger()
        .set_timestamp(ts.env.ledger().timestamp() + 86_400);
    amm.exec_multisig_emergency_wd(signers[0], &pid);
}
//
// These tests cover the tamper-resistant multisig guard, the announced
// delayed single-admin emergency withdrawal, and the distinct error
// variants introduced for the emergency path.

fn setup_pool_with_emergency_delay(fee_bps: i128, delay_secs: u64) -> TestSetup {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(12345);
    let admin = Address::generate(&env);
    let amm_addr = env.register_contract(None, AmmPool);
    let lp_addr = env.register_contract(None, LpToken);

    token::LpTokenClient::new(&env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(&env, "AMM LP Token"),
        &soroban_sdk::String::from_str(&env, "ALP"),
        &7u32,
    );

    let (ta, ta_sac) = create_sac(&env, &admin);
    let (tb, tb_sac) = create_sac(&env, &admin);

    AmmPoolClient::new(&env, &amm_addr).initialize_with_emergency_delay(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &fee_bps,
        &admin,
        &0_i128,
        &delay_secs,
    );

    let ta_addr = ta.address.clone();
    let tb_addr = tb.address.clone();
    drop((ta, ta_sac, tb, tb_sac));

    TestSetup {
        env,
        amm_addr,
        lp_addr,
        ta_addr,
        tb_addr,
        admin,
    }
}

fn seed_pool(ts: &TestSetup) -> i128 {
    let amm = AmmPoolClient::new(&ts.env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(&ts.env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(&ts.env, &ts.tb_addr);
    let provider = Address::generate(&ts.env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute()
}

#[test]
fn test_set_multisig_quorum_zero_rejected_when_multisig_active() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let s1 = Address::generate(env);
    let s2 = Address::generate(env);
    let s3 = Address::generate(env);
    let mut signers = soroban_sdk::Vec::new(env);
    signers.push_back(s1.clone());
    signers.push_back(s2.clone());
    signers.push_back(s3.clone());
    amm.set_multisig(&ts.admin, &signers, &2_u32);

    let empty = soroban_sdk::Vec::<Address>::new(env);
    let res = amm.try_set_multisig(&ts.admin, &empty, &0_u32);
    assert!(
        res.is_err(),
        "admin alone must not be able to disable an active multisig guard"
    );
}

#[test]
fn test_set_multisig_quorum_zero_via_multisig_succeeds() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let s1 = Address::generate(env);
    let s2 = Address::generate(env);
    let s3 = Address::generate(env);
    let mut signers = soroban_sdk::Vec::new(env);
    signers.push_back(s1.clone());
    signers.push_back(s2.clone());
    signers.push_back(s3.clone());
    amm.set_multisig(&ts.admin, &signers, &2_u32);

    let empty = soroban_sdk::Vec::<Address>::new(env);
    let pid = amm.propose_set_multisig(&s1, &empty, &0_u32);
    amm.approve_multisig_proposal(&s2, &pid);
    amm.exec_multisig_set_multisig(&s1, &pid);

    let (stored_signers, stored_quorum) = amm.get_multisig();
    assert_eq!(stored_quorum, 0);
    assert_eq!(stored_signers.len(), 0);
}

#[test]
fn test_set_multisig_invalid_quorum_exceeds_signers() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let s1 = Address::generate(env);
    let s2 = Address::generate(env);
    let s3 = Address::generate(env);
    let mut signers = soroban_sdk::Vec::new(env);
    signers.push_back(s1);
    signers.push_back(s2);
    signers.push_back(s3);
    let res = amm.try_set_multisig(&ts.admin, &signers, &4_u32);
    assert_eq!(res, Err(Ok(AmmError::InvalidQuorum)));
}

#[test]
fn test_emergency_withdraw_requires_pause() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    let to = Address::generate(env);
    amm.announce_emergency_withdraw(&ts.admin, &to);
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + 86_400 + 1);
    let res = amm.try_emergency_withdraw(&ts.admin, &to);
    assert_eq!(res, Err(Ok(AmmError::Paused)));
}

#[test]
fn test_emergency_withdraw_requires_announcement() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let to = Address::generate(env);
    let res = amm.try_emergency_withdraw(&ts.admin, &to);
    assert_eq!(res, Err(Ok(AmmError::NoPendingProposal)));
}

#[test]
fn test_emergency_withdraw_before_delay_fails() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let to = Address::generate(env);
    amm.announce_emergency_withdraw(&ts.admin, &to);
    let res = amm.try_emergency_withdraw(&ts.admin, &to);
    assert_eq!(res, Err(Ok(AmmError::EmergencyDelayNotElapsed)));
}

#[test]
fn test_emergency_withdraw_after_delay_succeeds_and_sweeps_fees() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    // Accrue protocol fees by swapping with a fee recipient configured.
    amm.set_protocol_fee(&ts.admin, &ts.admin, &10_i128);
    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    Swap::new(&amm, &trader, &ts.ta_addr, 100_000).execute();
    let (accrued_a, accrued_b) = amm.get_accrued_fees();
    assert!(accrued_a > 0 || accrued_b > 0);

    amm.pause();
    let to = Address::generate(env);
    amm.announce_emergency_withdraw(&ts.admin, &to);
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + 86_400 + 1);
    amm.emergency_withdraw(&ts.admin, &to);

    let bal_a = StellarTokenClient::new(env, &ts.ta_addr).balance(&to);
    let bal_b = StellarTokenClient::new(env, &ts.tb_addr).balance(&to);
    let pool_bal_a = StellarTokenClient::new(env, &ts.ta_addr).balance(&ts.amm_addr);
    let pool_bal_b = StellarTokenClient::new(env, &ts.tb_addr).balance(&ts.amm_addr);
    assert!(bal_a > 0 && bal_b > 0);
    assert_eq!(pool_bal_a, 0);
    assert_eq!(pool_bal_b, 0);
}

#[test]
fn test_emergency_withdraw_rejects_pool_as_recipient() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let res = amm.try_announce_emergency_withdraw(&ts.admin, &ts.amm_addr);
    assert_eq!(res, Err(Ok(AmmError::InvalidRecipient)));
}

#[test]
fn test_emergency_withdraw_rejects_lp_token_as_recipient() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let res = amm.try_announce_emergency_withdraw(&ts.admin, &ts.lp_addr);
    assert_eq!(res, Err(Ok(AmmError::InvalidRecipient)));
}

#[test]
fn test_cancel_emergency_withdraw_resets_announcement() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let to = Address::generate(env);
    amm.announce_emergency_withdraw(&ts.admin, &to);
    amm.cancel_emergency_withdraw(&ts.admin);
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + 86_400 + 1);
    let res = amm.try_emergency_withdraw(&ts.admin, &to);
    assert_eq!(res, Err(Ok(AmmError::NoPendingProposal)));
}

#[test]
fn test_second_announce_restarts_delay() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let to = Address::generate(env);
    amm.announce_emergency_withdraw(&ts.admin, &to);
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + 40_000);
    amm.announce_emergency_withdraw(&ts.admin, &to);
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + 40_000);
    let res = amm.try_emergency_withdraw(&ts.admin, &to);
    assert_eq!(res, Err(Ok(AmmError::EmergencyDelayNotElapsed)));
}

#[test]
fn test_emergency_delay_capped_at_seven_days() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let amm_addr = env.register_contract(None, AmmPool);
    let lp_addr = env.register_contract(None, LpToken);
    token::LpTokenClient::new(&env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(&env, "LP"),
        &soroban_sdk::String::from_str(&env, "LP"),
        &7u32,
    );
    let (ta, _) = create_sac(&env, &admin);
    let (tb, _) = create_sac(&env, &admin);
    let res = AmmPoolClient::new(&env, &amm_addr).try_initialize_with_emergency_delay(
        &admin,
        &ta.address,
        &tb.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
        &(7 * 86_400 + 1),
    );
    assert!(res.is_err(), "delay above 7 days must be rejected");
}

#[test]
fn test_exec_multisig_emergency_wd_quorum_not_reached() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    let s1 = Address::generate(env);
    let s2 = Address::generate(env);
    let mut signers = soroban_sdk::Vec::new(env);
    signers.push_back(s1.clone());
    signers.push_back(s2.clone());
    amm.set_multisig(&ts.admin, &signers, &2_u32);

    let to = Address::generate(env);
    let pid = amm.propose_emergency_withdraw(&s1, &to);
    let res = amm.try_exec_multisig_emergency_wd(&s1, &pid);
    assert_eq!(res, Err(Ok(AmmError::QuorumNotReached)));
}

#[test]
fn test_exec_multisig_emergency_wd_no_pending_proposal() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    let s1 = Address::generate(env);
    let s2 = Address::generate(env);
    let mut signers = soroban_sdk::Vec::new(env);
    signers.push_back(s1.clone());
    signers.push_back(s2.clone());
    amm.set_multisig(&ts.admin, &signers, &2_u32);

    let res = amm.try_exec_multisig_emergency_wd(&s1, &999_u64);
    assert_eq!(res, Err(Ok(AmmError::NoPendingProposal)));
}

#[test]
fn test_exec_multisig_emergency_wd_requires_delay_after_quorum() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let s1 = Address::generate(env);
    let s2 = Address::generate(env);
    let mut signers = soroban_sdk::Vec::new(env);
    signers.push_back(s1.clone());
    signers.push_back(s2.clone());
    amm.set_multisig(&ts.admin, &signers, &2_u32);

    let to = Address::generate(env);
    let pid = amm.propose_emergency_withdraw(&s1, &to);
    amm.approve_multisig_proposal(&s2, &pid);
    let res = amm.try_exec_multisig_emergency_wd(&s1, &pid);
    assert_eq!(res, Err(Ok(AmmError::EmergencyDelayNotElapsed)));
}

#[test]
fn test_exec_multisig_emergency_wd_after_delay_succeeds() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let s1 = Address::generate(env);
    let s2 = Address::generate(env);
    let mut signers = soroban_sdk::Vec::new(env);
    signers.push_back(s1.clone());
    signers.push_back(s2.clone());
    amm.set_multisig(&ts.admin, &signers, &2_u32);

    let to = Address::generate(env);
    let pid = amm.propose_emergency_withdraw(&s1, &to);
    amm.approve_multisig_proposal(&s2, &pid);
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + 86_400 + 1);
    amm.exec_multisig_emergency_wd(&s1, &pid);

    let bal_a = StellarTokenClient::new(env, &ts.ta_addr).balance(&to);
    let bal_b = StellarTokenClient::new(env, &ts.tb_addr).balance(&to);
    assert!(bal_a > 0 && bal_b > 0);
}

#[test]
fn test_exec_multisig_emergency_wd_rejects_pool_recipient() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let s1 = Address::generate(env);
    let s2 = Address::generate(env);
    let mut signers = soroban_sdk::Vec::new(env);
    signers.push_back(s1.clone());
    signers.push_back(s2.clone());
    amm.set_multisig(&ts.admin, &signers, &2_u32);

    let res = amm.try_propose_emergency_withdraw(&s1, &ts.amm_addr);
    assert_eq!(res, Err(Ok(AmmError::InvalidRecipient)));
}

#[test]
fn test_exec_multisig_emergency_wd_rejects_lp_token_recipient() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let s1 = Address::generate(env);
    let s2 = Address::generate(env);
    let mut signers = soroban_sdk::Vec::new(env);
    signers.push_back(s1.clone());
    signers.push_back(s2.clone());
    amm.set_multisig(&ts.admin, &signers, &2_u32);

    let res = amm.try_propose_emergency_withdraw(&s1, &ts.lp_addr);
    assert_eq!(res, Err(Ok(AmmError::InvalidRecipient)));
}

#[test]
fn test_announce_emergency_withdraw_emits_event() {
    use soroban_sdk::testutils::Events as _;
    use soroban_sdk::IntoVal;

    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let to = Address::generate(env);
    amm.announce_emergency_withdraw(&ts.admin, &to);

    let events = env.events().all();
    let found = events.iter().any(|e| {
        e.0 == amm.address && e.1 == (Symbol::new(env, "emergency_announce"),).into_val(env)
    });
    assert!(found, "emergency_announce event must be emitted");
}

#[test]
fn test_cancel_emergency_withdraw_emits_event() {
    use soroban_sdk::testutils::Events as _;
    use soroban_sdk::IntoVal;

    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let to = Address::generate(env);
    amm.announce_emergency_withdraw(&ts.admin, &to);
    amm.cancel_emergency_withdraw(&ts.admin);

    let events = env.events().all();
    let found = events.iter().any(|e| {
        e.0 == amm.address && e.1 == (Symbol::new(env, "emergency_cancel"),).into_val(env)
    });
    assert!(found, "emergency_cancel event must be emitted");
}

#[test]
fn test_emergency_withdraw_emits_event_with_amounts() {
    use soroban_sdk::testutils::Events as _;
    use soroban_sdk::IntoVal;

    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let to = Address::generate(env);
    amm.announce_emergency_withdraw(&ts.admin, &to);
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + 86_400 + 1);
    amm.emergency_withdraw(&ts.admin, &to);

    let events = env.events().all();
    let found = events
        .iter()
        .any(|e| e.0 == amm.address && e.1 == (Symbol::new(env, "emergency_wd"),).into_val(env));
    assert!(found, "emergency_wd event must be emitted");
}

#[test]
fn test_emergency_withdraw_clears_announcement_after_execution() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let to = Address::generate(env);
    amm.announce_emergency_withdraw(&ts.admin, &to);
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + 86_400 + 1);
    amm.emergency_withdraw(&ts.admin, &to);
    let res = amm.try_emergency_withdraw(&ts.admin, &to);
    assert_eq!(res, Err(Ok(AmmError::NoPendingProposal)));
}

#[test]
fn test_emergency_withdraw_announcement_recipient_must_match() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let to = Address::generate(env);
    let other = Address::generate(env);
    amm.announce_emergency_withdraw(&ts.admin, &to);
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + 86_400 + 1);
    let res = amm.try_emergency_withdraw(&ts.admin, &other);
    assert!(res.is_err(), "recipient must match the announced address");
}

#[test]
fn test_emergency_withdraw_non_admin_rejected() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let to = Address::generate(env);
    let attacker = Address::generate(env);
    let res = amm.try_announce_emergency_withdraw(&attacker, &to);
    assert!(res.is_err(), "only admin may announce");
}

#[test]
fn test_default_emergency_delay_is_24h() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let to = Address::generate(env);
    amm.announce_emergency_withdraw(&ts.admin, &to);
    // One second short of 24h must still fail.
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + 86_400 - 1);
    let res = amm.try_emergency_withdraw(&ts.admin, &to);
    assert_eq!(res, Err(Ok(AmmError::EmergencyDelayNotElapsed)));
    // Exactly at 24h must succeed.
    env.ledger().set_timestamp(env.ledger().timestamp() + 1);
    amm.emergency_withdraw(&ts.admin, &to);
}

#[test]
fn test_custom_emergency_delay_respected() {
    let ts = setup_pool_with_emergency_delay(30, 3_600);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    seed_pool(&ts);
    amm.pause();
    let to = Address::generate(env);
    amm.announce_emergency_withdraw(&ts.admin, &to);
    env.ledger().set_timestamp(env.ledger().timestamp() + 3_599);
    let res = amm.try_emergency_withdraw(&ts.admin, &to);
    assert_eq!(res, Err(Ok(AmmError::EmergencyDelayNotElapsed)));
    env.ledger().set_timestamp(env.ledger().timestamp() + 1);
    amm.emergency_withdraw(&ts.admin, &to);
}

#[test]
fn test_set_multisig_quorum_one_of_three_rejected_when_active() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let s1 = Address::generate(env);
    let s2 = Address::generate(env);
    let s3 = Address::generate(env);
    let mut signers = soroban_sdk::Vec::new(env);
    signers.push_back(s1.clone());
    signers.push_back(s2.clone());
    signers.push_back(s3.clone());
    amm.set_multisig(&ts.admin, &signers, &2_u32);

    // Admin alone cannot weaken the guard to a single controlled signer.
    let mut one = soroban_sdk::Vec::new(env);
    one.push_back(s1.clone());
    let res = amm.try_set_multisig(&ts.admin, &one, &1_u32);
    assert!(
        res.is_err(),
        "admin alone must not weaken an active multisig guard"
    );
}

#[test]
fn test_multisig_emergency_withdraw_sweeps_accrued_fees() {
    let ts = setup_pool(30);
    let env = &ts.env;
    let amm = AmmPoolClient::new(env, &ts.amm_addr);
    let ta_sac = StellarAssetClient::new(env, &ts.ta_addr);
    let tb_sac = StellarAssetClient::new(env, &ts.tb_addr);
    let provider = Address::generate(env);
    ta_sac.mint(&provider, &1_000_000_i128);
    tb_sac.mint(&provider, &1_000_000_i128);
    AddLiquidity::new(&amm, &provider, 1_000_000, 1_000_000).execute();

    amm.set_protocol_fee(&ts.admin, &ts.admin, &10_i128);
    let trader = Address::generate(env);
    ta_sac.mint(&trader, &100_000_i128);
    Swap::new(&amm, &trader, &ts.ta_addr, 100_000).execute();
    let (accrued_a, accrued_b) = amm.get_accrued_fees();
    assert!(accrued_a > 0 || accrued_b > 0);

    let s1 = Address::generate(env);
    let s2 = Address::generate(env);
    let mut signers = soroban_sdk::Vec::new(env);
    signers.push_back(s1.clone());
    signers.push_back(s2.clone());
    amm.set_multisig(&ts.admin, &signers, &2_u32);
    amm.pause();

    let to = Address::generate(env);
    let pid = amm.propose_emergency_withdraw(&s1, &to);
    amm.approve_multisig_proposal(&s2, &pid);
    env.ledger()
        .set_timestamp(env.ledger().timestamp() + 86_400 + 1);
    amm.exec_multisig_emergency_wd(&s1, &pid);

    let pool_bal_a = StellarTokenClient::new(env, &ts.ta_addr).balance(&ts.amm_addr);
    let pool_bal_b = StellarTokenClient::new(env, &ts.tb_addr).balance(&ts.amm_addr);
    assert_eq!(pool_bal_a, 0, "accrued token_a fees must be swept");
    assert_eq!(pool_bal_b, 0, "accrued token_b fees must be swept");
}

// ── add_liquidity_fot minimum-liquidity lock (#503, restored by #1096) ───────
//
// Regression note: commit 22736ea9 fixed `add_liquidity_fot` and added the
// three `test_fot_*` tests below, but a later rewrite of contracts/amm replaced
// both the function body and the tests with the older duplicated block. CI
// stayed green because no remaining test exercised a first deposit through
// the FOT entrypoint. Both entrypoints now share `compute_liquidity_mint`, so
// the first-deposit rule cannot diverge again.

pub(crate) use fot_token::{FotToken, FotTokenClient};

mod fot_token {
    extern crate std;

    use super::*;

    /// A token that burns 2% of every transferred amount (fee-on-transfer).
    #[contract]
    pub struct FotToken;

    #[contracttype]
    enum FotKey {
        Balance(Address),
    }

    #[contractimpl]
    impl FotToken {
        pub fn mint(env: Env, to: Address, amount: i128) {
            let bal = Self::balance(env.clone(), to.clone());
            env.storage()
                .instance()
                .set(&FotKey::Balance(to), &(bal + amount));
        }

        pub fn balance(env: Env, id: Address) -> i128 {
            env.storage()
                .instance()
                .get(&FotKey::Balance(id))
                .unwrap_or(0)
        }

        pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
            from.require_auth();
            let from_bal = Self::balance(env.clone(), from.clone());
            let to_bal = Self::balance(env.clone(), to.clone());
            let received = amount - amount * 2 / 100;
            env.storage()
                .instance()
                .set(&FotKey::Balance(from), &(from_bal - amount));
            env.storage()
                .instance()
                .set(&FotKey::Balance(to), &(to_bal + received));
        }
    }
}

fn fot_setup(fee_bps: i128) -> (TestSetup, Address) {
    let ts = setup_pool(fee_bps);
    let provider = Address::generate(&ts.env);
    StellarAssetClient::new(&ts.env, &ts.ta_addr).mint(&provider, &100_000_000);
    StellarAssetClient::new(&ts.env, &ts.tb_addr).mint(&provider, &100_000_000);
    (ts, provider)
}

fn fot_deposit(
    amm: &AmmPoolClient,
    provider: &Address,
    a: i128,
    b: i128,
) -> Result<Result<i128, soroban_sdk::Error>, Result<AmmError, soroban_sdk::InvokeError>> {
    amm.try_add_liquidity_fot(provider, &a, &b, &0, &0, &0, &u64::MAX)
}

#[test]
fn test_fot_first_deposit_locks_minimum_liquidity() {
    let (ts, provider) = fot_setup(30);
    let amm = AmmPoolClient::new(&ts.env, &ts.amm_addr);
    let shares = fot_deposit(&amm, &provider, 2_000_000, 2_000_000)
        .unwrap()
        .unwrap();
    assert_eq!(shares, 1_999_000);
    assert_eq!(amm.get_info().total_shares, 2_000_000);
    let lp = token::LpTokenClient::new(&ts.env, &ts.lp_addr);
    assert_eq!(lp.balance(&ts.amm_addr), 1_000);
    assert_eq!(lp.balance(&provider), 1_999_000);
}

#[test]
fn test_fot_first_deposit_rejects_insufficient_shares() {
    let (ts, provider) = fot_setup(30);
    let amm = AmmPoolClient::new(&ts.env, &ts.amm_addr);
    assert_eq!(
        fot_deposit(&amm, &provider, 1_000, 1_000),
        Err(Ok(AmmError::InsufficientShares))
    );
    assert_eq!(
        fot_deposit(&amm, &provider, 500, 500),
        Err(Ok(AmmError::InsufficientShares))
    );
}

#[test]
fn test_fot_subsequent_deposit_no_extra_lock() {
    let (ts, provider) = fot_setup(30);
    let amm = AmmPoolClient::new(&ts.env, &ts.amm_addr);
    let s1 = fot_deposit(&amm, &provider, 2_000_000, 2_000_000)
        .unwrap()
        .unwrap();
    let s2 = fot_deposit(&amm, &provider, 1_000_000, 1_000_000)
        .unwrap()
        .unwrap();
    let lp = token::LpTokenClient::new(&ts.env, &ts.lp_addr);
    assert_eq!(lp.balance(&ts.amm_addr), 1_000);
    assert_eq!(lp.balance(&provider), s1 + s2);
    assert_eq!(amm.get_info().total_shares, s1 + s2 + 1_000);
}

#[test]
fn test_fot_first_deposit_one_share_seed_rejected() {
    let (ts, provider) = fot_setup(30);
    let amm = AmmPoolClient::new(&ts.env, &ts.amm_addr);
    assert_eq!(
        fot_deposit(&amm, &provider, 1, 1),
        Err(Ok(AmmError::InsufficientShares))
    );
    assert_eq!(amm.get_info().total_shares, 0);
}

#[test]
fn test_fot_first_lp_cannot_drain_pool() {
    let (ts, provider) = fot_setup(30);
    let amm = AmmPoolClient::new(&ts.env, &ts.amm_addr);
    let shares = fot_deposit(&amm, &provider, 1_000_000, 1_000_000)
        .unwrap()
        .unwrap();
    assert_eq!(shares, 999_000);
    amm.remove_liquidity(&provider, &shares, &0, &0, &u64::MAX);
    let info = amm.get_info();
    assert_eq!(info.total_shares, 1_000);
    assert_eq!(info.reserve_a, 1_000);
    assert_eq!(info.reserve_b, 1_000);
    // The locked shares belong to the pool and cannot be redeemed.
    assert!(amm
        .try_remove_liquidity(&provider, &1, &0, &0, &u64::MAX)
        .is_err());
}

#[test]
fn test_fot_min_shares_compares_provider_share_after_lock() {
    let (ts, provider) = fot_setup(30);
    let amm = AmmPoolClient::new(&ts.env, &ts.amm_addr);
    // sqrt = 1_000_000, provider share after lock = 999_000.
    let too_high = amm.try_add_liquidity_fot(
        &provider,
        &1_000_000,
        &1_000_000,
        &0,
        &0,
        &1_000_000,
        &u64::MAX,
    );
    assert_eq!(too_high, Err(Ok(AmmError::SlippageExceeded)));
    let ok = amm.add_liquidity_fot(
        &provider,
        &1_000_000,
        &1_000_000,
        &0,
        &0,
        &999_000,
        &u64::MAX,
    );
    assert_eq!(ok, 999_000);
}

#[test]
fn test_fot_zero_reserve_with_shares_returns_typed_error() {
    let (ts, provider) = fot_setup(30);
    let amm = AmmPoolClient::new(&ts.env, &ts.amm_addr);
    fot_deposit(&amm, &provider, 1_000_000, 1_000_000)
        .unwrap()
        .unwrap();
    ts.env.as_contract(&ts.amm_addr, || {
        ts.env.storage().instance().set(&DataKey::ReserveA, &0_i128);
    });
    assert_eq!(
        fot_deposit(&amm, &provider, 1_000, 1_000),
        Err(Ok(AmmError::EmptyPool))
    );
    assert_eq!(
        amm.try_add_liquidity(&provider, &1_000, &1_000, &0, &u64::MAX),
        Err(Ok(AmmError::EmptyPool))
    );
}

fn last_add_liquidity_event(env: &Env, pool: &Address) -> (i128, i128, i128) {
    use soroban_sdk::TryFromVal;
    let events = env.events().all();
    let mut found = None;
    for (addr, _topics, data) in events.iter() {
        if &addr == pool {
            found = Some(data);
        }
    }
    let (_, tuple) =
        <(u32, (i128, i128, i128))>::try_from_val(env, &found.expect("pool event")).unwrap();
    tuple
}

/// Parametrised: `add_liquidity` and `add_liquidity_fot` must agree on shares,
/// reserves and the emitted event for plain tokens, on empty and funded pools.
#[test]
fn test_add_liquidity_and_fot_are_equivalent() {
    let cases: [(i128, i128, bool); 4] = [
        (1_000_000, 1_000_000, false),
        (4_000_000, 1_000_000, false),
        (1_000_000, 1_000_000, true),
        (3_000_000, 2_000_000, true),
    ];
    for (a, b, prefund) in cases {
        let mut results = std::vec::Vec::new();
        for use_fot in [false, true] {
            let (ts, provider) = fot_setup(30);
            let amm = AmmPoolClient::new(&ts.env, &ts.amm_addr);
            if prefund {
                amm.add_liquidity(&provider, &5_000_000, &5_000_000, &0, &u64::MAX);
            }
            let shares = if use_fot {
                amm.add_liquidity_fot(&provider, &a, &b, &0, &0, &0, &u64::MAX)
            } else {
                amm.add_liquidity(&provider, &a, &b, &0, &u64::MAX)
            };
            let info = amm.get_info();
            let ev = last_add_liquidity_event(&ts.env, &ts.amm_addr);
            results.push((
                shares,
                info.reserve_a,
                info.reserve_b,
                info.total_shares,
                ev,
            ));
        }
        assert_eq!(results[0], results[1], "case ({a}, {b}, {prefund})");
    }
}

#[test]
fn test_fot_token_two_percent_deduction_mints_from_actual_and_locks() {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(12345);
    let admin = Address::generate(&env);
    let amm_addr = env.register_contract(None, AmmPool);
    let lp_addr = env.register_contract(None, LpToken);
    token::LpTokenClient::new(&env, &lp_addr).initialize(
        &amm_addr,
        &soroban_sdk::String::from_str(&env, "AMM LP Token"),
        &soroban_sdk::String::from_str(&env, "ALP"),
        &7u32,
    );
    let fot_addr = env.register_contract(None, FotToken);
    let (tb, tb_sac) = create_sac(&env, &admin);
    let amm = AmmPoolClient::new(&env, &amm_addr);
    amm.initialize(
        &admin,
        &fot_addr,
        &tb.address,
        &lp_addr,
        &30_i128,
        &admin,
        &0_i128,
    );
    let provider = Address::generate(&env);
    FotTokenClient::new(&env, &fot_addr).mint(&provider, &2_000_000);
    tb_sac.mint(&provider, &1_960_000);

    // Pool receives 98% of 2_000_000 = 1_960_000 of token A.
    let shares = amm.add_liquidity_fot(
        &provider,
        &2_000_000,
        &1_960_000,
        &1_960_000,
        &0,
        &0,
        &u64::MAX,
    );
    // sqrt(1_960_000 * 1_960_000) = 1_960_000, minus the 1_000 lock.
    assert_eq!(shares, 1_959_000);
    let info = amm.get_info();
    assert_eq!(info.reserve_a, 1_960_000);
    assert_eq!(info.total_shares, 1_960_000);
    let lp = token::LpTokenClient::new(&env, &lp_addr);
    assert_eq!(lp.balance(&amm_addr), 1_000);
}
