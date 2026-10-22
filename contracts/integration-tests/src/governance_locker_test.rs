//! Governance voting against real authorization (issue #986).
//!
//! `governance::vote` locks the voter's LP tokens through `LpToken::lock`,
//! which requires the token's locker to authorise. Every other governance
//! test runs under `mock_all_auths`, which satisfies that check whoever the
//! locker is, so none of them could notice that no production path ever made
//! governance the locker — and on a real network every vote trapped.
//!
//! These tests never call `mock_all_auths`. Each user-signed call is covered
//! by an explicit `MockAuth` for exactly that invocation tree, so the
//! contract-to-contract steps (governance -> pool `set_lp_locker`, pool -> LP
//! token `set_locker`, governance -> LP token `lock` / `unlock`) have to be
//! authorised the way they would be on-chain: by the calling contract being
//! the direct invoker.

use soroban_sdk::testutils::{Address as _, Ledger, MockAuth, MockAuthInvoke};
use soroban_sdk::token::StellarAssetClient;
use soroban_sdk::{Address, BytesN, Env, IntoVal, Val, Vec};

use amm::{AmmError, AmmPoolClient};
use factory::FactoryClient;
use governance::{GovernanceClient, ProposalKind, ProposalStatus, Vote};
use token::LpTokenClient;

const DEADLINE: u64 = u64::MAX;
const DEPOSIT: i128 = 1_000_000;

/// Authorise `address` for exactly one invocation of `contract.fn_name(args)`
/// with the given sub-invocations, and nothing else.
fn allow(
    env: &Env,
    address: &Address,
    contract: &Address,
    fn_name: &'static str,
    args: Vec<Val>,
    sub_invokes: &[MockAuthInvoke],
) {
    env.mock_auths(&[MockAuth {
        address,
        invoke: &MockAuthInvoke {
            contract,
            fn_name,
            args,
            sub_invokes,
        },
    }]);
}

struct Pool {
    env: Env,
    admin: Address,
    token_a: Address,
    token_b: Address,
    pool: Address,
    lp: Address,
}

impl Pool {
    fn amm(&self) -> AmmPoolClient<'_> {
        AmmPoolClient::new(&self.env, &self.pool)
    }

    fn lp(&self) -> LpTokenClient<'_> {
        LpTokenClient::new(&self.env, &self.lp)
    }

    /// Mint both pool tokens to `holder` and deposit them, signed by the
    /// token admin and the holder respectively, then close the ledger so
    /// the resulting LP balance is visible to a proposal snapshot.
    fn provide_liquidity(&self, holder: &Address) {
        let env = &self.env;
        for token in [&self.token_a, &self.token_b] {
            allow(
                env,
                &self.admin,
                token,
                "mint",
                (holder, DEPOSIT).into_val(env),
                &[],
            );
            StellarAssetClient::new(env, token).mint(holder, &DEPOSIT);
        }

        let transfer_a = MockAuthInvoke {
            contract: &self.token_a,
            fn_name: "transfer",
            args: (holder, &self.pool, DEPOSIT).into_val(env),
            sub_invokes: &[],
        };
        let transfer_b = MockAuthInvoke {
            contract: &self.token_b,
            fn_name: "transfer",
            args: (holder, &self.pool, DEPOSIT).into_val(env),
            sub_invokes: &[],
        };
        allow(
            env,
            holder,
            &self.pool,
            "add_liquidity",
            (holder, DEPOSIT, DEPOSIT, 1_i128, DEADLINE).into_val(env),
            &[transfer_a, transfer_b],
        );
        self.amm()
            .add_liquidity(holder, &DEPOSIT, &DEPOSIT, &1_i128, &DEADLINE);

        env.ledger().with_mut(|l| l.sequence_number += 1);
    }
}

fn new_env() -> Env {
    let env = Env::default();
    env.budget().reset_unlimited();
    env.ledger().with_mut(|l| {
        l.timestamp = 1_000_000;
        l.sequence_number = 100;
    });
    env
}

/// A factory whose admin is `admin`, with AMM and token WASM registered.
fn deploy_factory(env: &Env, admin: &Address) -> FactoryClient<'static> {
    let amm_hash: BytesN<32> = env.deployer().upload_contract_wasm(amm::WASM);
    let token_hash: BytesN<32> = env.deployer().upload_contract_wasm(token::WASM);
    let factory = FactoryClient::new(env, &env.register_contract(None, factory::Factory));
    allow(
        env,
        admin,
        &factory.address,
        "initialize",
        (admin, amm_hash.clone(), token_hash.clone()).into_val(env),
        &[],
    );
    factory.initialize(admin, &amm_hash, &token_hash);
    factory
}

/// Create a pool through the factory, signed by the factory admin.
fn create_pool(env: &Env, gov_hash: Option<BytesN<32>>) -> (Pool, Option<Address>) {
    let admin = Address::generate(env);
    let factory = deploy_factory(env, &admin);
    let token_a = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_b = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    allow(
        env,
        &admin,
        &factory.address,
        "create_pool",
        (&admin, &token_a, &token_b, 2_i128, gov_hash.clone()).into_val(env),
        &[],
    );
    let (pool, gov) = factory.create_pool(&admin, &token_a, &token_b, &2_i128, &gov_hash);
    let lp = factory.get_lp_token(&pool).unwrap();
    // The factory normalises the pair; keep the pool's own order.
    let info = AmmPoolClient::new(env, &pool).get_info();

    (
        Pool {
            env: env.clone(),
            admin,
            token_a: info.token_a,
            token_b: info.token_b,
            pool,
            lp,
        },
        gov,
    )
}

#[test]
fn factory_pool_with_governance_can_propose_vote_execute_and_unlock() {
    let env = new_env();
    let gov_hash: BytesN<32> = env.deployer().upload_contract_wasm(governance::WASM);
    let (pool, gov) = create_pool(&env, Some(gov_hash));
    let gov = GovernanceClient::new(&env, &gov.expect("governance deployed"));

    // create_pool wired the locker without any extra signature.
    assert_eq!(pool.lp().locker(), gov.address);

    let voter = Address::generate(&env);
    pool.provide_liquidity(&voter);
    let lp_balance = pool.lp().balance(&voter);
    assert!(lp_balance > 0);

    let kind = ProposalKind::UpdateFee(50);
    allow(
        &env,
        &voter,
        &gov.address,
        "propose",
        (&voter, kind.clone()).into_val(&env),
        &[],
    );
    let proposal_id = gov.propose(&voter, &kind);

    // The vote locks the voter's LP tokens through LpToken::lock. Only the
    // voter signs; governance authorises the lock as the token's locker.
    allow(
        &env,
        &voter,
        &gov.address,
        "vote",
        (&voter, proposal_id, Vote::For).into_val(&env),
        &[],
    );
    gov.vote(&voter, &proposal_id, &Vote::For);

    let proposal = gov.get_proposal(&proposal_id);
    assert_eq!(proposal.votes_for, lp_balance, "vote recorded");
    assert_eq!(
        pool.lp().locked_balance(&voter),
        lp_balance,
        "LP tokens locked"
    );

    // Locked tokens cannot move while the vote stands.
    let other = Address::generate(&env);
    allow(
        &env,
        &voter,
        &pool.lp,
        "transfer",
        (&voter, &other, 1_i128).into_val(&env),
        &[],
    );
    assert!(pool.lp().try_transfer(&voter, &other, &1).is_err());

    // Execute after the voting period and timelock; governance applies the
    // fee change as the pool admin.
    env.ledger()
        .with_mut(|l| l.timestamp = proposal.execute_after + 1);
    gov.execute(&proposal_id);
    assert_eq!(gov.proposal_status(&proposal_id), ProposalStatus::Executed);
    assert_eq!(pool.amm().get_info().fee_bps, 50);

    // unlock_vote releases the tokens through LpToken::unlock, authorised by
    // governance as the locker that locked them.
    allow(
        &env,
        &voter,
        &gov.address,
        "unlock_vote",
        (&voter, proposal_id).into_val(&env),
        &[],
    );
    gov.unlock_vote(&voter, &proposal_id);
    assert_eq!(pool.lp().locked_balance(&voter), 0);
}

#[test]
fn standalone_governance_is_wired_by_the_pool_admin() {
    // The deploy script path: a pool created without governance (admin is
    // an account), and a governance contract deployed and initialised
    // separately against it.
    let env = new_env();
    let (pool, gov) = create_pool(&env, None);
    assert!(gov.is_none());
    assert_eq!(
        pool.lp().locker(),
        pool.pool,
        "LP token starts with the pool as locker"
    );

    let gov = GovernanceClient::new(&env, &env.register_contract(None, governance::Governance));
    gov.initialize(
        &pool.admin,
        &pool.pool,
        &pool.lp,
        &3_600_u64,
        &3_600_u64,
        &1_000_i128,
        &100_i128,
    );

    // Governance is not the pool admin, so it cannot claim the locker...
    assert!(gov.try_claim_lp_locker().is_err());
    // ...and nobody but the pool admin can have the pool delegate it.
    let stranger = Address::generate(&env);
    allow(
        &env,
        &stranger,
        &pool.pool,
        "set_lp_locker",
        (&gov.address,).into_val(&env),
        &[],
    );
    assert!(pool.amm().try_set_lp_locker(&gov.address).is_err());
    assert_eq!(pool.lp().locker(), pool.pool);

    allow(
        &env,
        &pool.admin,
        &pool.pool,
        "set_lp_locker",
        (&gov.address,).into_val(&env),
        &[],
    );
    pool.amm().set_lp_locker(&gov.address);
    assert_eq!(pool.lp().locker(), gov.address);

    let voter = Address::generate(&env);
    pool.provide_liquidity(&voter);
    let kind = ProposalKind::UpdateFee(40);
    allow(
        &env,
        &voter,
        &gov.address,
        "propose",
        (&voter, kind.clone()).into_val(&env),
        &[],
    );
    let proposal_id = gov.propose(&voter, &kind);
    allow(
        &env,
        &voter,
        &gov.address,
        "vote",
        (&voter, proposal_id, Vote::Against).into_val(&env),
        &[],
    );
    gov.vote(&voter, &proposal_id, &Vote::Against);
    let locked = pool.lp().locked_balance(&voter);
    assert_eq!(locked, pool.lp().balance(&voter));

    // A defeated proposal releases the lock too.
    let vote_end = gov.get_proposal(&proposal_id).vote_end;
    env.ledger().with_mut(|l| l.timestamp = vote_end + 1);
    assert_eq!(gov.proposal_status(&proposal_id), ProposalStatus::Defeated);
    allow(
        &env,
        &voter,
        &gov.address,
        "unlock_vote",
        (&voter, proposal_id).into_val(&env),
        &[],
    );
    gov.unlock_vote(&voter, &proposal_id);
    assert_eq!(pool.lp().locked_balance(&voter), 0);
}

#[test]
fn claim_lp_locker_is_idempotent() {
    let env = new_env();
    let gov_hash: BytesN<32> = env.deployer().upload_contract_wasm(governance::WASM);
    let (pool, gov) = create_pool(&env, Some(gov_hash));
    let gov = GovernanceClient::new(&env, &gov.unwrap());
    // Anyone may call it; it can only ever point the locker at governance.
    gov.claim_lp_locker();
    assert_eq!(pool.lp().locker(), gov.address);
}

/// #1107: a governance `UpdateFee` that would make the swap fee equal to the
/// pool's protocol fee is refused when it executes, and the fee stays as it was.
#[test]
fn update_fee_proposal_reaching_the_equal_fee_state_fails_at_execute() {
    let env = new_env();
    let gov_hash: BytesN<32> = env.deployer().upload_contract_wasm(governance::WASM);
    let (pool, gov) = create_pool(&env, Some(gov_hash));
    let gov = GovernanceClient::new(&env, &gov.expect("governance deployed"));

    let voter = Address::generate(&env);
    pool.provide_liquidity(&voter);

    // What follows is about the pool's fee rules, not signatures.
    env.mock_all_auths();
    let fee_before = pool.amm().get_info().fee_bps;
    assert_eq!(fee_before, 30);

    // Governance is the pool admin; it turns on a 20 bps protocol cut.
    pool.amm()
        .set_protocol_fee(&gov.address, &pool.admin, &20_i128);

    // A proposal to make the swap fee 20 bps would hand LPs nothing.
    let kind = ProposalKind::UpdateFee(20);
    let proposal_id = gov.propose(&voter, &kind);
    gov.vote(&voter, &proposal_id, &Vote::For);
    let proposal = gov.get_proposal(&proposal_id);
    env.ledger()
        .with_mut(|l| l.timestamp = proposal.execute_after + 1);

    assert!(gov.try_execute(&proposal_id).is_err());
    assert_ne!(
        gov.proposal_status(&proposal_id),
        ProposalStatus::Executed,
        "a failed execution must not mark the proposal executed"
    );
    let info = pool.amm().get_info();
    assert_eq!(info.fee_bps, fee_before, "pool fee must be unchanged");
    assert_eq!(info.protocol_fee_bps, 20);

    // The pool itself names the reason.
    assert_eq!(
        pool.amm().try_update_fee(&20_i128),
        Err(Ok(AmmError::InvalidFeeBps))
    );
}
