//! Decodes the events the real `amm` contract publishes.
//!
//! Each test performs an operation on a deployed pool, takes the pool's most
//! recent event from `env.events().all()`, and checks that `decode_amm_event`
//! returns the variant and field values the operation implies. The payloads are
//! produced by the contract's own emit sites, so changing an emit site without
//! updating the decoder fails here. The hand-written payloads in
//! `src/events.rs` cannot catch that on their own.

use amm::{AmmPool, AmmPoolClient};
use soroban_amm_sdk::events::{
    decode_amm_event, AddLiquidityEvent, AdminChangedEvent, AdminNominatedEvent, AmmEvent,
    FeeUpdatedEvent, FlashFeeUpdatedEvent, FlashLoanEvent, ProtocolFeeSetEvent,
    RemoveLiquidityEvent, RemoveLiquidityOneSidedEvent, SwapEvent,
};
use soroban_sdk::{
    contract, contractimpl, symbol_short,
    testutils::{Address as _, Events, Ledger},
    token::{StellarAssetClient, TokenClient},
    Address, Bytes, Env, String,
};
use token::{LpToken, LpTokenClient};

const DEADLINE: u64 = 1_000_000;
const SEED: i128 = 1_000_000;

/// Repays a flash loan in full (principal plus fee) so the pool's balance
/// check passes. It needs the fee tokens in its own balance before the call.
#[contract]
pub struct RepayingReceiver;

#[contractimpl]
impl RepayingReceiver {
    pub fn initialize(env: Env, pool: Address, token_a: Address, token_b: Address) {
        env.storage().instance().set(&symbol_short!("pool"), &pool);
        env.storage().instance().set(&symbol_short!("ta"), &token_a);
        env.storage().instance().set(&symbol_short!("tb"), &token_b);
    }

    pub fn on_flash_loan(
        env: Env,
        token_a_amount: i128,
        token_b_amount: i128,
        fee_a: i128,
        fee_b: i128,
        _data: Bytes,
    ) -> bool {
        let pool: Address = env
            .storage()
            .instance()
            .get(&symbol_short!("pool"))
            .unwrap();
        let token_a: Address = env.storage().instance().get(&symbol_short!("ta")).unwrap();
        let token_b: Address = env.storage().instance().get(&symbol_short!("tb")).unwrap();
        let me = env.current_contract_address();
        TokenClient::new(&env, &token_a).transfer(&me, &pool, &(token_a_amount + fee_a));
        TokenClient::new(&env, &token_b).transfer(&me, &pool, &(token_b_amount + fee_b));
        true
    }
}

struct Setup {
    env: Env,
    pool: Address,
    admin: Address,
    token_a: Address,
    token_b: Address,
}

fn setup() -> Setup {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(12_345);

    let admin = Address::generate(&env);
    let pool = env.register_contract(None, AmmPool);
    let lp_token = env.register_contract(None, LpToken);
    LpTokenClient::new(&env, &lp_token).initialize(
        &pool,
        &String::from_str(&env, "AMM LP Token"),
        &String::from_str(&env, "ALP"),
        &7u32,
    );

    let token_a = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_b = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    AmmPoolClient::new(&env, &pool).initialize(
        &admin, &token_a, &token_b, &lp_token, &30_i128, &admin, &0_i128,
    );

    Setup {
        env,
        pool,
        admin,
        token_a,
        token_b,
    }
}

impl Setup {
    fn mint(&self, to: &Address, amount: i128) {
        StellarAssetClient::new(&self.env, &self.token_a).mint(to, &amount);
        StellarAssetClient::new(&self.env, &self.token_b).mint(to, &amount);
    }

    /// Decodes the most recent event the pool published. Events from the token
    /// contracts (transfers, mints) are skipped.
    fn last_pool_event(&self) -> AmmEvent {
        let events = self.env.events().all();
        let (_, topics, data) = events
            .iter()
            .rev()
            .find(|(contract, _, _)| *contract == self.pool)
            .expect("pool published no event");
        decode_amm_event(&self.env, topics, data).expect("pool event did not decode")
    }

    /// Funds one provider with `SEED` of each token and seeds the pool.
    fn seeded_provider(&self) -> (Address, i128) {
        let provider = Address::generate(&self.env);
        self.mint(&provider, SEED);
        let amm = AmmPoolClient::new(&self.env, &self.pool);
        let shares = amm.add_liquidity(&provider, &SEED, &SEED, &0_i128, &DEADLINE);
        (provider, shares)
    }
}

#[test]
fn decodes_add_liquidity() {
    let s = setup();
    let provider = Address::generate(&s.env);
    s.mint(&provider, SEED);

    let shares = AmmPoolClient::new(&s.env, &s.pool)
        .add_liquidity(&provider, &SEED, &SEED, &0_i128, &DEADLINE);

    assert_eq!(
        s.last_pool_event(),
        AmmEvent::AddLiquidity(AddLiquidityEvent {
            provider,
            amount_a: SEED,
            amount_b: SEED,
            shares_minted: shares,
        })
    );
}

#[test]
fn decodes_swap() {
    let s = setup();
    let _ = s.seeded_provider();
    let trader = Address::generate(&s.env);
    s.mint(&trader, SEED);

    let amount_out = AmmPoolClient::new(&s.env, &s.pool).swap(
        &trader,
        &s.token_a,
        &10_000_i128,
        &0_i128,
        &DEADLINE,
    );

    assert_eq!(
        s.last_pool_event(),
        AmmEvent::Swap(SwapEvent {
            trader,
            token_in: s.token_a.clone(),
            amount_in: 10_000,
            token_out: s.token_b.clone(),
            amount_out,
            referrer: None,
        })
    );
}

#[test]
fn decodes_swap_exact_out() {
    let s = setup();
    let _ = s.seeded_provider();
    let trader = Address::generate(&s.env);
    s.mint(&trader, SEED);

    let amount_in = AmmPoolClient::new(&s.env, &s.pool).swap_exact_out(
        &trader,
        &s.token_b,
        &5_000_i128,
        &50_000_i128,
        &DEADLINE,
    );

    assert_eq!(
        s.last_pool_event(),
        AmmEvent::Swap(SwapEvent {
            trader,
            token_in: s.token_a.clone(),
            amount_in,
            token_out: s.token_b.clone(),
            amount_out: 5_000,
            referrer: None,
        })
    );
}

#[test]
fn decodes_swap_fot_with_referrer() {
    let s = setup();
    let _ = s.seeded_provider();
    let trader = Address::generate(&s.env);
    let referrer = Address::generate(&s.env);
    s.mint(&trader, SEED);

    // With a standard token `actual_received` equals `amount_in`, and the
    // event still carries the referrer as its fifth field.
    let (amount_out, actual_received) = AmmPoolClient::new(&s.env, &s.pool).swap_fot(
        &trader,
        &s.token_a,
        &10_000_i128,
        &0_i128,
        &0_i128,
        &DEADLINE,
        &Some(referrer.clone()),
    );

    assert_eq!(
        s.last_pool_event(),
        AmmEvent::Swap(SwapEvent {
            trader,
            token_in: s.token_a.clone(),
            amount_in: actual_received,
            token_out: s.token_b.clone(),
            amount_out,
            referrer: Some(referrer),
        })
    );
}

#[test]
fn decodes_flash_loan() {
    let s = setup();
    let _ = s.seeded_provider();
    let receiver = s.env.register_contract(None, RepayingReceiver);
    RepayingReceiverClient::new(&s.env, &receiver).initialize(&s.pool, &s.token_a, &s.token_b);
    s.mint(&receiver, 10_000);

    let (fee_a, fee_b) = AmmPoolClient::new(&s.env, &s.pool).flash_loan(
        &receiver,
        &1_000_i128,
        &500_i128,
        &Bytes::new(&s.env),
    );

    assert_eq!(
        s.last_pool_event(),
        AmmEvent::FlashLoan(FlashLoanEvent {
            receiver,
            amount_a: 1_000,
            amount_b: 500,
            fee_a,
            fee_b,
        })
    );
}

#[test]
fn decodes_remove_liquidity() {
    let s = setup();
    let (provider, shares) = s.seeded_provider();
    let burn = shares / 4;

    let (amount_a, amount_b) = AmmPoolClient::new(&s.env, &s.pool)
        .remove_liquidity(&provider, &burn, &0_i128, &0_i128, &DEADLINE);

    assert_eq!(
        s.last_pool_event(),
        AmmEvent::RemoveLiquidity(RemoveLiquidityEvent {
            provider,
            shares_burned: burn,
            amount_a,
            amount_b,
        })
    );
}

#[test]
fn decodes_remove_liquidity_one_sided() {
    let s = setup();
    let (provider, shares) = s.seeded_provider();
    let burn = shares / 4;

    let total_out = AmmPoolClient::new(&s.env, &s.pool)
        .remove_liquidity_one_sided(&provider, &burn, &s.token_a, &0_i128, &DEADLINE);

    assert_eq!(
        s.last_pool_event(),
        AmmEvent::RemoveLiquidityOneSided(RemoveLiquidityOneSidedEvent {
            provider,
            shares_burned: burn,
            token_out: s.token_a.clone(),
            total_out,
        })
    );
}

#[test]
fn decodes_fee_and_flash_fee_updates() {
    let s = setup();
    let amm = AmmPoolClient::new(&s.env, &s.pool);

    amm.update_fee(&25_i128);
    assert_eq!(
        s.last_pool_event(),
        AmmEvent::FeeUpdated(FeeUpdatedEvent {
            admin: s.admin.clone(),
            new_fee_bps: 25,
        })
    );

    amm.update_flash_loan_fee(&9_i128);
    assert_eq!(
        s.last_pool_event(),
        AmmEvent::FlashFeeUpdated(FlashFeeUpdatedEvent {
            admin: s.admin.clone(),
            new_fee_bps: 9,
        })
    );
}

#[test]
fn decodes_protocol_fee_set() {
    let s = setup();
    let recipient = Address::generate(&s.env);

    // The pool's swap fee is 30 bps and the protocol share must stay below it.
    AmmPoolClient::new(&s.env, &s.pool).set_protocol_fee(&s.admin, &recipient, &10_i128);

    assert_eq!(
        s.last_pool_event(),
        AmmEvent::ProtocolFeeSet(ProtocolFeeSetEvent {
            protocol_fee_bps: 10,
            recipient,
        })
    );
}

#[test]
fn decodes_admin_rotation() {
    let s = setup();
    let amm = AmmPoolClient::new(&s.env, &s.pool);
    let new_admin = Address::generate(&s.env);

    amm.propose_admin(&s.admin, &new_admin);
    assert_eq!(
        s.last_pool_event(),
        AmmEvent::AdminNominated(AdminNominatedEvent {
            current_admin: s.admin.clone(),
            new_admin: new_admin.clone(),
        })
    );

    amm.accept_admin(&new_admin);
    assert_eq!(
        s.last_pool_event(),
        AmmEvent::AdminChanged(AdminChangedEvent { new_admin })
    );
}
