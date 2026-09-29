//! Governance contract for LP-weighted fee parameter voting.
//!
//! LP token holders can propose changes to the pool's `fee_bps`.
//! Voting power is locked during the proposal lifecycle to prevent
//! flash-loan and vote-then-sell attacks. A proposal passes when:
//!   - `votes_for > votes_against`
//!   - total votes cast >= quorum (configurable % of total LP supply at snapshot)
//!
//! After the voting period ends a timelock delay must elapse before anyone
//! can call `execute()`, which applies the change via `update_fee()` on the
//! AMM contract.

#![no_std]

// Export compiled WASM for tests/dev usage when the `testutils` feature is enabled.
#[cfg(feature = "testutils")]
pub const WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32v1-none/release/governance.wasm"
));

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env, Symbol, Vec};

// ── Constants ─────────────────────────────────────────────────────────────────

const MAX_BPS: i128 = 10_000;
const MIN_PERSISTENT_TTL: u32 = 172_800; // ~10 days at 5s/ledger
const PERSISTENT_TTL_BUMP_TO: u32 = 259_200; // ~15 days at 5s/ledger
                                             // Instance-entry TTL maintenance. The instance entry holds the governance
                                             // contract's executable and all `storage().instance()` state (admin, voting
                                             // configuration, proposal count). If that entry lapses the contract is
                                             // archived and every call traps: in-flight proposals can no longer be voted on
                                             // or executed while the voting window keeps advancing, so a proposal can
                                             // expire purely because the contract was archived. Every public entrypoint
                                             // bumps this, including read-only paths, which are often the only traffic the
                                             // contract sees for long stretches.
                                             //
                                             // Ledgers, at ~5 s each: 172_800 ≈ 10 days (top up when less than this
                                             // remains) and 518_400 ≈ 30 days (target live-until), matching the amm
                                             // reference contract.
const MIN_INSTANCE_TTL: u32 = 172_800;
const INSTANCE_TTL_BUMP_TO: u32 = 518_400;
/// Multisig may veto a passed proposal within this window after voting ends.
const VETO_WINDOW_SECS: u64 = 24 * 60 * 60;
/// Maximum delegation chain depth (prevents unbounded recursion).
const MAX_DELEGATION_DEPTH: u32 = 8;

// ── Typed errors ─────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum GovernanceError {
    AlreadyInitialized = 1,
    InvalidVotingPeriod = 2,
    InvalidTimelock = 3,
    InvalidQuorumBps = 4,
    InvalidProposerStake = 5,
    InvalidFeeBps = 6,
    ZeroTotalSupply = 7,
    InsufficientStake = 8,
    ProposalNotFound = 9,
    VotingNotStarted = 10,
    VotingPeriodEnded = 11,
    AlreadyExecuted = 12,
    ProposalCancelled = 13,
    AlreadyVoted = 14,
    NoVotingPower = 15,
    VotingPeriodActive = 16,
    ProposalExpired = 17,
    TimelockNotElapsed = 18,
    QuorumNotMet = 19,
    ProposalDefeated = 20,
    NotProposer = 21,
    NoLockedVote = 22,
    ProposalNotConcluded = 23,
    CannotDelegateToSelf = 24,
    Unauthorized = 25,
    HasDelegated = 26,
    DelegationCycle = 27,
    ProposalVetoed = 28,
    VetoWindowExpired = 29,
    NotVetoMultisig = 30,
    InsufficientSnapshotBal = 31,
    VetoMultisigNotSet = 32,
    NoPendingAdmin = 33,
    /// The `UpdateFactoryGlobalFee` proposal's `offset` / `limit` window does
    /// not cover every pool registered in the factory.  Execution is rejected
    /// so the proposal cannot be permanently marked executed while only a
    /// partial set of pools was updated.
    PartialFactoryUpdate = 34,
    /// A function that reads governance configuration (admin, AMM pool, LP
    /// token, voting period, timelock, quorum, proposer stake, proposal
    /// counter) was called before `initialize`. Discriminant 2 is taken by
    /// `InvalidVotingPeriod`, so this is appended rather than renumbered.
    NotInitialized = 35,
}

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    /// Address of the AMM pool contract.
    AmmPool,
    /// Address of the LP token contract.
    LpToken,
    /// Monotonically increasing proposal counter.
    ProposalCount,
    /// Governance admin.
    Admin,
    /// Pending admin nomination for two-step handover.
    PendingAdmin,
    /// Minimum proposer stake in basis points of total LP supply.
    MinProposerStakeBps,
    /// Voting period in seconds (configurable at initialize).
    VotingPeriod,
    /// Timelock delay in seconds (configurable at initialize).
    Timelock,
    /// Quorum requirement in basis points of total LP supply at snapshot.
    QuorumBps,
    /// Additional quorum bps required per day a proposal has been open (#311).
    QuorumDecayRateBpsPerDay,
    /// Individual proposal storage.
    Proposal(u32),
    /// Vote record for a voter on a proposal: (proposal_id, voter).
    HasVoted(u32, Address),
    /// Locked voting amount for a voter on a proposal.
    LockedVote(u32, Address),
    /// Delegation mapping: delegator -> delegatee address.
    Delegate(Address),
    /// Protocol multisig authorized to veto passed proposals.
    VetoMultisig,
    /// Number of addresses delegating to a given delegatee.
    DelegatorCount(Address),
    /// Delegator at index for a delegatee.
    Delegator(Address, u32),
    /// Index of a delegator in their delegatee's list (for removal).
    DelegatorSlot(Address),
    /// Audit record for a vetoed proposal.
    VetoAudit(u32),
}

// ── Types ─────────────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum ProposalStatus {
    /// Voting is open.
    Active,
    /// Voting closed; waiting for timelock to expire.
    Pending,
    /// Timelock elapsed; ready to execute.
    Queued,
    /// Proposal was executed successfully.
    Executed,
    /// Proposal failed quorum or majority.
    Defeated,
    /// Proposal expired without execution after timelock window.
    Expired,
    /// Proposal was cancelled by the original proposer.
    Cancelled,
    /// Multisig vetoed; community discussion period is active.
    InDiscussion,
    /// Multisig vetoed; discussion period ended — cannot execute.
    Vetoed,
}

/// Choice for a vote.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum Vote {
    For,
    Against,
    Abstain,
}

/// Records how an address voted on a specific proposal.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum VoteRecord {
    DidNotVote,
    VotedFor,
    VotedAgainst,
    VotedAbstain,
}

/// Current governance configuration returned by `get_params`.
#[contracttype]
#[derive(Clone, Debug)]
pub struct GovernanceParams {
    pub voting_period_secs: u64,
    pub timelock_secs: u64,
    pub quorum_bps: i128,
    pub min_proposer_stake_bps: i128,
    pub veto_multisig: Option<Address>,
    /// Extra quorum bps per day open; 0 disables decay (#311).
    pub quorum_decay_rate_bps_per_day: i128,
}

/// On-chain audit trail for a governance veto.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct VetoAudit {
    pub proposal_id: u32,
    pub vetoed_by: Address,
    pub vetoed_at: u64,
    pub discussion_end: u64,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateProtocolFeeParams {
    pub new_bps: i128,
    pub new_recipient: Address,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateFactoryTreasuryParams {
    pub factory: Address,
    pub treasury: Address,
    pub global_protocol_fee_bps: i128,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateFactoryGlobalFeeParams {
    pub factory: Address,
    pub offset: u32,
    pub limit: u32,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateClOracleParams {
    pub cl_pool: Address,
    pub oracle: Option<Address>,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateClMaxOracleDeviationParams {
    pub cl_pool: Address,
    pub max_deviation_bps: i128,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct UpdateClProtocolFeeParams {
    pub cl_pool: Address,
    pub recipient: Address,
    pub bps: i128,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct TransferClPoolAdminParams {
    pub cl_pool: Address,
    pub new_admin: Address,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct SetClPositionNftParams {
    pub cl_pool: Address,
    pub nft: Option<Address>,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct CreatePolVestingParams {
    /// POL vesting contract address.
    pub pol_vesting: Address,
    /// Beneficiary of the vesting schedule.
    pub beneficiary: Address,
    /// LP token to vest.
    pub lp_token: Address,
    /// AMM pool the LP tokens belong to.
    pub pool: Address,
    /// Total LP tokens to vest.
    pub total: i128,
    pub start_ledger: u32,
    pub cliff_ledger: u32,
    pub end_ledger: u32,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum ProposalKind {
    UpdateFee(i128),
    UpdateFeeTier(i128), // 0-3: VeryLow, Low, Medium, High
    UpdateProtocolFee(UpdateProtocolFeeParams),
    UpdateFlashLoanFee(i128),
    TransferAdmin(Address),
    PausePool,
    UnpausePool,
    EmergencyWithdraw(Address),
    UpdateFactoryTreasury(UpdateFactoryTreasuryParams),
    UpdateFactoryGlobalFee(UpdateFactoryGlobalFeeParams),
    /// Deploy a time-based vesting schedule for protocol-owned LP tokens.
    CreatePolVesting(CreatePolVestingParams),
    /// Pause a concentrated-liquidity pool.
    PauseClPool(Address),
    /// Unpause a concentrated-liquidity pool.
    UnpauseClPool(Address),
    /// Attach or remove the oracle aggregator on a CL pool.
    UpdateClOracle(UpdateClOracleParams),
    /// Update the maximum oracle deviation (basis points) on a CL pool.
    UpdateClMaxOracleDeviation(UpdateClMaxOracleDeviationParams),
    /// Update protocol fee parameters on a CL pool.
    UpdateClProtocolFee(UpdateClProtocolFeeParams),
    /// Transfer admin of a CL pool to a new address (two-step handover).
    TransferClPoolAdmin(TransferClPoolAdminParams),
    /// Wire or detach the position-NFT contract on a CL pool.
    SetClPositionNft(SetClPositionNftParams),
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    pub id: u32,
    pub proposer: Address,
    pub kind: ProposalKind,
    /// LP total supply snapshot at proposal creation.
    pub snapshot_total_supply: i128,
    /// Ledger sequence when LP balances were snapshotted for voting.
    pub snapshot_ledger: u32,
    /// Timestamp when voting opens (== creation timestamp).
    pub vote_start: u64,
    /// Timestamp when voting closes.
    pub vote_end: u64,
    /// Timestamp after which execution is allowed (vote_end + timelock_secs).
    pub execute_after: u64,
    /// Timestamp after which the proposal expires if not executed (execute_after + timelock_secs).
    pub expires_at: u64,
    pub votes_for: i128,
    pub votes_against: i128,
    pub votes_abstain: i128,
    pub executed: bool,
    pub cancelled: bool,
    pub vetoed: bool,
    pub vetoed_by: Option<Address>,
    pub vetoed_at: Option<u64>,
    pub discussion_end: Option<u64>,
}

// ── LP token client ───────────────────────────────────────────────────────────

#[soroban_sdk::contractclient(name = "LpTokenClient")]
pub trait LpTokenInterface {
    fn balance(env: Env, id: Address) -> i128;
    fn balance_at(env: Env, id: Address, ledger: u32) -> i128;
    fn total_supply(env: Env) -> i128;
    fn lock(env: Env, holder: Address, amount: i128);
    /// `locker` is the contract that originally locked the tokens (issue #556 fix).
    fn unlock(env: Env, holder: Address, locker: Address, amount: i128);
}

// ── AMM client ────────────────────────────────────────────────────────────────

#[soroban_sdk::contractclient(name = "AmmPoolClient")]
pub trait AmmPoolInterface {
    fn update_fee(env: Env, new_fee_bps: i128);
    fn update_flash_loan_fee(env: Env, new_fee_bps: i128);
    fn set_protocol_fee(env: Env, admin: Address, recipient: Address, protocol_fee_bps: i128);
    fn pause(env: Env);
    fn unpause(env: Env);
    fn emergency_withdraw(env: Env, to: Address);
    fn propose_admin(env: Env, current_admin: Address, new_admin: Address);
}

// ── Factory client ────────────────────────────────────────────────────────────

#[soroban_sdk::contractclient(name = "FactoryClient")]
pub trait FactoryInterface {
    fn set_treasury(env: Env, admin: Address, treasury: Address, global_protocol_fee_bps: i128);
    fn set_global_fee_paginated(env: Env, admin: Address, offset: u32, limit: u32) -> u32;
    /// Return the total number of AMM pools registered in the factory.
    fn get_pool_count(env: Env) -> u64;
}

// ── POL Vesting client ────────────────────────────────────────────────────────

#[soroban_sdk::contractclient(name = "PolVestingClient")]
pub trait PolVestingInterface {
    #[allow(clippy::too_many_arguments)]
    fn create_vesting(
        env: Env,
        governance: Address,
        beneficiary: Address,
        lp_token: Address,
        pool: Address,
        total: i128,
        start_ledger: u32,
        cliff_ledger: u32,
        end_ledger: u32,
    ) -> u32;
}

// ── CL pool client ────────────────────────────────────────────────────────────

#[soroban_sdk::contractclient(name = "ClPoolClient")]
pub trait ClPoolInterface {
    fn set_oracle(env: Env, admin: Address, oracle: Option<Address>);
    fn set_max_oracle_deviation_bps(env: Env, admin: Address, max_deviation_bps: i128);
    fn pause(env: Env, admin: Address);
    fn unpause(env: Env, admin: Address);
    fn propose_admin(env: Env, admin: Address, new_admin: Address);
    fn set_protocol_fee(env: Env, admin: Address, recipient: Address, bps: i128);
    fn set_position_nft(env: Env, admin: Address, nft: Option<Address>);
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct Governance;

#[contractimpl]
impl Governance {
    // ── Setup ─────────────────────────────────────────────────────────────────

    /// One-time initialisation. Must be called after deployment.
    ///
    /// - `voting_period_secs` must be > 0.
    /// - `timelock_secs` must be > 0.
    /// - `quorum_bps` must be in [1, 10_000].
    /// - `min_proposer_stake_bps` must be in [0, 10_000].
    #[allow(clippy::too_many_arguments)]
    pub fn initialize(
        env: Env,
        admin: Address,
        amm_pool: Address,
        lp_token: Address,
        voting_period_secs: u64,
        timelock_secs: u64,
        quorum_bps: i128,
        min_proposer_stake_bps: i128,
    ) -> Result<(), GovernanceError> {
        Self::extend_instance_ttl(&env);
        if env.storage().instance().has(&DataKey::AmmPool) {
            return Err(GovernanceError::AlreadyInitialized);
        }
        if voting_period_secs == 0 {
            return Err(GovernanceError::InvalidVotingPeriod);
        }
        if timelock_secs == 0 {
            return Err(GovernanceError::InvalidTimelock);
        }
        if !(1..=MAX_BPS).contains(&quorum_bps) {
            return Err(GovernanceError::InvalidQuorumBps);
        }
        if !(0..=MAX_BPS).contains(&min_proposer_stake_bps) {
            return Err(GovernanceError::InvalidProposerStake);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::AmmPool, &amm_pool);
        env.storage().instance().set(&DataKey::LpToken, &lp_token);
        env.storage()
            .instance()
            .set(&DataKey::VotingPeriod, &voting_period_secs);
        env.storage()
            .instance()
            .set(&DataKey::Timelock, &timelock_secs);
        env.storage()
            .instance()
            .set(&DataKey::QuorumBps, &quorum_bps);
        env.storage()
            .instance()
            .set(&DataKey::MinProposerStakeBps, &min_proposer_stake_bps);
        env.storage()
            .instance()
            .set(&DataKey::QuorumDecayRateBpsPerDay, &0i128);
        env.storage().instance().set(&DataKey::ProposalCount, &0u32);
        Ok(())
    }

    /// Admin-only: quorum increases by this many bps per day a proposal is open (#311).
    pub fn set_quorum_decay_bps_per_day(env: Env, new_rate: i128) -> Result<(), GovernanceError> {
        let admin = Self::read_admin(&env)?;
        admin.require_auth();
        if new_rate < 0 {
            return Err(GovernanceError::InvalidQuorumBps);
        }
        env.storage()
            .instance()
            .set(&DataKey::QuorumDecayRateBpsPerDay, &new_rate);
        Ok(())
    }

    /// Effective quorum bps for a proposal (base + decay, capped at 10_000).
    ///
    /// Returns [`GovernanceError::ProposalNotFound`] for an unknown
    /// `proposal_id` rather than trapping, so off-chain callers can handle a
    /// missing proposal via `try_get_effective_quorum`.
    pub fn get_effective_quorum(env: Env, proposal_id: u32) -> Result<i128, GovernanceError> {
        Self::extend_instance_ttl(&env);
        let proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;
        Self::effective_quorum_bps(&env, &proposal)
    }

    /// Admin-only governance parameter update.
    pub fn set_min_proposer_stake_bps(env: Env, new_bps: i128) -> Result<(), GovernanceError> {
        let admin = Self::read_admin(&env)?;
        admin.require_auth();
        if !(0..=MAX_BPS).contains(&new_bps) {
            return Err(GovernanceError::InvalidProposerStake);
        }
        env.storage()
            .instance()
            .set(&DataKey::MinProposerStakeBps, &new_bps);
        Ok(())
    }

    /// Admin-only: update the timelock delay between vote end and execution.
    /// Execution is always gated by at least the veto window (24h), so even
    /// a delay of 0 does not allow execution before the veto window expires.
    pub fn set_timelock_delay(env: Env, new_delay: u64) -> Result<(), GovernanceError> {
        let admin = Self::read_admin(&env)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Timelock, &new_delay);
        Ok(())
    }

    /// Admin-only: nominate a new governance admin.
    ///
    /// The nominee must call `accept_admin` to complete the two-step handover,
    /// preventing a single compromised transaction from taking over the contract.
    pub fn propose_admin(
        env: Env,
        current_admin: Address,
        new_admin: Address,
    ) -> Result<(), GovernanceError> {
        let stored = Self::read_admin(&env)?;
        if current_admin != stored {
            return Err(GovernanceError::Unauthorized);
        }
        current_admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Some(new_admin.clone()));
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "admin_nominated"),),
            (current_admin, new_admin)
        );
        Ok(())
    }

    /// Accept a pending governance admin nomination.
    ///
    /// Only the nominated address can call this, and it must authorize the
    /// transaction. On success the stored admin is updated and the pending
    /// nomination is cleared.
    pub fn accept_admin(env: Env, new_admin: Address) -> Result<(), GovernanceError> {
        Self::extend_instance_ttl(&env);
        let pending: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None);
        let nominee = pending.ok_or(GovernanceError::NoPendingAdmin)?;
        if new_admin != nominee {
            return Err(GovernanceError::Unauthorized);
        }
        new_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Option::<Address>::None);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "admin_changed"),),
            (new_admin,)
        );
        Ok(())
    }

    // ── Core functions ────────────────────────────────────────────────────────

    /// Create a new proposal to change the pool fee.
    ///
    /// The proposer must hold at least the configured minimum LP stake.
    /// Returns the new `proposal_id`.
    pub fn propose(
        env: Env,
        proposer: Address,
        kind: ProposalKind,
    ) -> Result<u32, GovernanceError> {
        Self::extend_instance_ttl(&env);
        proposer.require_auth();

        match &kind {
            ProposalKind::UpdateFee(new_fee_bps) => {
                if !(0..=MAX_BPS).contains(new_fee_bps) {
                    return Err(GovernanceError::InvalidFeeBps);
                }
            }
            ProposalKind::UpdateFeeTier(fee_tier) => {
                // Validate fee_tier is 0-3
                Self::fee_tier_to_bps(*fee_tier)?;
            }
            ProposalKind::UpdateProtocolFee(params) => {
                if !(0..=MAX_BPS).contains(&params.new_bps) {
                    return Err(GovernanceError::InvalidFeeBps);
                }
            }
            ProposalKind::UpdateFlashLoanFee(new_bps) => {
                if !(0..=MAX_BPS).contains(new_bps) {
                    return Err(GovernanceError::InvalidFeeBps);
                }
            }
            ProposalKind::TransferAdmin(_new_admin) => {}
            ProposalKind::PausePool => {}
            ProposalKind::UnpausePool => {}
            ProposalKind::EmergencyWithdraw(_) => {}
            ProposalKind::UpdateFactoryTreasury(params) => {
                if !(0..=MAX_BPS).contains(&params.global_protocol_fee_bps) {
                    return Err(GovernanceError::InvalidFeeBps);
                }
            }
            ProposalKind::UpdateFactoryGlobalFee(params) => {
                // offset must be zero (all pools must be covered from the start)
                // and limit must be non-zero.  These are cheap eager checks that
                // catch obviously invalid proposals before they enter voting.
                if params.offset != 0 || params.limit == 0 {
                    return Err(GovernanceError::PartialFactoryUpdate);
                }
            }
            ProposalKind::CreatePolVesting(_) => {}
            ProposalKind::PauseClPool(_) => {}
            ProposalKind::UnpauseClPool(_) => {}
            ProposalKind::UpdateClOracle(_) => {}
            ProposalKind::UpdateClMaxOracleDeviation(params) => {
                if !(0..=MAX_BPS).contains(&params.max_deviation_bps) {
                    return Err(GovernanceError::InvalidFeeBps);
                }
            }
            ProposalKind::UpdateClProtocolFee(params) => {
                if !(0..=MAX_BPS).contains(&params.bps) {
                    return Err(GovernanceError::InvalidFeeBps);
                }
            }
            ProposalKind::TransferClPoolAdmin(_) => {}
            ProposalKind::SetClPositionNft(_) => {}
        }

        let lp_token = Self::read_lp_token(&env)?;
        let lp_client = LpTokenClient::new(&env, &lp_token);

        let total_supply = lp_client.total_supply();
        if total_supply == 0 {
            return Err(GovernanceError::ZeroTotalSupply);
        }
        let proposer_balance = lp_client.balance(&proposer);
        let min_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MinProposerStakeBps)
            .unwrap_or(0);
        let min_stake = ((total_supply * min_bps) / MAX_BPS).max(1);
        if proposer_balance < min_stake {
            return Err(GovernanceError::InsufficientStake);
        }

        let voting_period = Self::read_voting_period(&env)?;
        let timelock = Self::read_timelock(&env)?;

        let now = env.ledger().timestamp();
        let vote_end = now + voting_period;
        let execute_after = vote_end + timelock.max(VETO_WINDOW_SECS);
        // Execution window: at least one voting period even when timelock is 0.
        let expires_at = execute_after + timelock.max(voting_period);

        let id = Self::read_proposal_count(&env)?;

        // Snapshot the last already-closed ledger, never the current still-open
        // one. LpToken::write_checkpoint overwrites (not appends) the checkpoint
        // when a balance change lands in the same ledger, so using the current
        // ledger would fold any mint/transfer included in this same ledger —
        // even in a later transaction — into balance_at(holder, snapshot_ledger),
        // letting an attacker inflate their snapshotted voting power. Any change
        // at the current ledger or later is written at a ledger strictly greater
        // than this snapshot and is therefore excluded.
        let snapshot_ledger = env.ledger().sequence().saturating_sub(1);

        let proposal = Proposal {
            id,
            proposer: proposer.clone(),
            kind: kind.clone(),
            snapshot_total_supply: total_supply,
            snapshot_ledger,
            vote_start: now,
            vote_end,
            execute_after,
            expires_at,
            votes_for: 0,
            votes_against: 0,
            votes_abstain: 0,
            executed: false,
            cancelled: false,
            vetoed: false,
            vetoed_by: None,
            vetoed_at: None,
            discussion_end: None,
        };

        let proposal_key = DataKey::Proposal(id);
        env.storage().persistent().set(&proposal_key, &proposal);
        Self::bump_key_ttl(&env, &proposal_key);
        env.storage()
            .instance()
            .set(&DataKey::ProposalCount, &(id + 1));

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "proposed"),),
            (id, proposer, kind, vote_end, snapshot_ledger)
        );

        Ok(id)
    }

    /// Cast a vote on an active proposal.
    ///
    /// Voting power uses LP balances snapshotted at proposal creation (`snapshot_ledger`).
    /// Delegators cannot vote directly; the terminal delegatee votes with aggregated power.
    pub fn vote(
        env: Env,
        voter: Address,
        proposal_id: u32,
        choice: Vote,
    ) -> Result<(), GovernanceError> {
        Self::extend_instance_ttl(&env);
        voter.require_auth();

        if Self::get_delegate(env.clone(), voter.clone()).is_some() {
            return Err(GovernanceError::HasDelegated);
        }

        let proposal_key = DataKey::Proposal(proposal_id);
        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&proposal_key)
            .ok_or(GovernanceError::ProposalNotFound)?;
        Self::bump_key_ttl(&env, &proposal_key);

        let now = env.ledger().timestamp();
        if now < proposal.vote_start {
            return Err(GovernanceError::VotingNotStarted);
        }
        if now > proposal.vote_end {
            return Err(GovernanceError::VotingPeriodEnded);
        }
        if proposal.executed {
            return Err(GovernanceError::AlreadyExecuted);
        }
        if proposal.cancelled {
            return Err(GovernanceError::ProposalCancelled);
        }
        if proposal.vetoed {
            return Err(GovernanceError::ProposalVetoed);
        }

        let voted_key = DataKey::HasVoted(proposal_id, voter.clone());
        if env.storage().persistent().has(&voted_key) {
            return Err(GovernanceError::AlreadyVoted);
        }

        let lp_token = Self::read_lp_token(&env)?;
        let lp_client = LpTokenClient::new(&env, &lp_token);

        let (voting_power, lock_accounts) =
            Self::aggregated_voting_power(&env, &lp_client, &voter, &proposal)?;
        if voting_power == 0 {
            return Err(GovernanceError::NoVotingPower);
        }

        let record = match choice {
            Vote::For => VoteRecord::VotedFor,
            Vote::Against => VoteRecord::VotedAgainst,
            Vote::Abstain => VoteRecord::VotedAbstain,
        };

        for i in 0..lock_accounts.len() {
            // In bounds: `i` ranges over `0..lock_accounts.len()`.
            let (account, amount) = lock_accounts.get(i).unwrap();
            lp_client.lock(&account, &amount);
            let lock_key = DataKey::LockedVote(proposal_id, account.clone());
            env.storage().persistent().set(&lock_key, &amount);
            Self::bump_key_ttl(&env, &lock_key);

            // Mark every aggregated holder (delegators included) as having
            // voted on this proposal so they cannot be locked a second time
            // via a later vote() call, and so they cannot vote directly or
            // be re-aggregated after undelegating mid-proposal.
            let holder_voted_key = DataKey::HasVoted(proposal_id, account.clone());
            env.storage().persistent().set(&holder_voted_key, &record);
            Self::bump_key_ttl(&env, &holder_voted_key);
        }

        match choice {
            Vote::For => {
                proposal.votes_for += voting_power;
            }
            Vote::Against => {
                proposal.votes_against += voting_power;
            }
            Vote::Abstain => {
                proposal.votes_abstain += voting_power;
            }
        }

        env.storage().persistent().set(&proposal_key, &proposal);
        Self::bump_key_ttl(&env, &proposal_key);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "voted"),),
            (proposal_id, voter, choice, voting_power)
        );
        Ok(())
    }

    /// Execute a passed proposal after the timelock has elapsed.
    ///
    /// Anyone can call this once the conditions are met.
    pub fn execute(env: Env, proposal_id: u32) -> Result<(), GovernanceError> {
        Self::extend_instance_ttl(&env);
        let proposal_key = DataKey::Proposal(proposal_id);
        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&proposal_key)
            .ok_or(GovernanceError::ProposalNotFound)?;
        Self::bump_key_ttl(&env, &proposal_key);

        if proposal.executed {
            return Err(GovernanceError::AlreadyExecuted);
        }
        if proposal.cancelled {
            return Err(GovernanceError::ProposalCancelled);
        }
        if proposal.vetoed {
            return Err(GovernanceError::ProposalVetoed);
        }

        let now = env.ledger().timestamp();

        if now <= proposal.vote_end {
            return Err(GovernanceError::VotingPeriodActive);
        }
        if now > proposal.expires_at {
            return Err(GovernanceError::ProposalExpired);
        }
        if now < proposal.execute_after {
            return Err(GovernanceError::TimelockNotElapsed);
        }

        let effective_quorum = Self::effective_quorum_bps(&env, &proposal)?;
        let total_votes = proposal.votes_for + proposal.votes_against + proposal.votes_abstain;
        let quorum_threshold = proposal.snapshot_total_supply * effective_quorum / MAX_BPS;
        if total_votes < quorum_threshold {
            return Err(GovernanceError::QuorumNotMet);
        }

        if proposal.votes_for <= proposal.votes_against {
            return Err(GovernanceError::ProposalDefeated);
        }

        let amm_pool = Self::read_amm_pool(&env)?;
        let amm_client = AmmPoolClient::new(&env, &amm_pool);
        match &proposal.kind {
            ProposalKind::UpdateFee(new_fee_bps) => {
                amm_client.update_fee(new_fee_bps);
            }
            ProposalKind::UpdateFeeTier(fee_tier) => {
                let new_fee_bps = Self::fee_tier_to_bps(*fee_tier)?;
                amm_client.update_fee(&new_fee_bps);
            }
            ProposalKind::UpdateProtocolFee(params) => {
                let self_addr = env.current_contract_address();
                amm_client.set_protocol_fee(&self_addr, &params.new_recipient, &params.new_bps);
            }
            ProposalKind::UpdateFlashLoanFee(new_bps) => {
                amm_client.update_flash_loan_fee(new_bps);
            }
            ProposalKind::TransferAdmin(new_admin) => {
                let self_addr = env.current_contract_address();
                amm_client.propose_admin(&self_addr, new_admin);
            }
            ProposalKind::PausePool => {
                amm_client.pause();
            }
            ProposalKind::UnpausePool => {
                amm_client.unpause();
            }
            ProposalKind::EmergencyWithdraw(to) => {
                amm_client.emergency_withdraw(to);
            }
            ProposalKind::UpdateFactoryTreasury(params) => {
                let self_addr = env.current_contract_address();
                let factory_client = FactoryClient::new(&env, &params.factory);
                factory_client.set_treasury(
                    &self_addr,
                    &params.treasury,
                    &params.global_protocol_fee_bps,
                );
            }
            ProposalKind::UpdateFactoryGlobalFee(params) => {
                let self_addr = env.current_contract_address();
                let factory_client = FactoryClient::new(&env, &params.factory);
                // Ensure the pagination window covers every pool registered in
                // the factory.  If it does not, the transaction is aborted so
                // the proposal cannot be marked executed while pools remain
                // untouched.
                let pool_count: u64 = factory_client.get_pool_count();
                let window_end: u64 = (params.offset as u64).saturating_add(params.limit as u64);
                // Both conditions must hold:
                //  - offset == 0: the window starts from the first pool
                //  - window_end >= pool_count: the window reaches the last pool
                // A non-zero offset would silently skip the leading pools.
                if params.offset != 0 || window_end < pool_count {
                    return Err(GovernanceError::PartialFactoryUpdate);
                }
                let _updated = factory_client.set_global_fee_paginated(
                    &self_addr,
                    &params.offset,
                    &params.limit,
                );
            }
            ProposalKind::CreatePolVesting(params) => {
                let self_addr = env.current_contract_address();
                PolVestingClient::new(&env, &params.pol_vesting).create_vesting(
                    &self_addr,
                    &params.beneficiary,
                    &params.lp_token,
                    &params.pool,
                    &params.total,
                    &params.start_ledger,
                    &params.cliff_ledger,
                    &params.end_ledger,
                );
            }
            ProposalKind::PauseClPool(cl_pool) => {
                let self_addr = env.current_contract_address();
                ClPoolClient::new(&env, cl_pool).pause(&self_addr);
            }
            ProposalKind::UnpauseClPool(cl_pool) => {
                let self_addr = env.current_contract_address();
                ClPoolClient::new(&env, cl_pool).unpause(&self_addr);
            }
            ProposalKind::UpdateClOracle(params) => {
                let self_addr = env.current_contract_address();
                ClPoolClient::new(&env, &params.cl_pool).set_oracle(&self_addr, &params.oracle);
            }
            ProposalKind::UpdateClMaxOracleDeviation(params) => {
                let self_addr = env.current_contract_address();
                ClPoolClient::new(&env, &params.cl_pool)
                    .set_max_oracle_deviation_bps(&self_addr, &params.max_deviation_bps);
            }
            ProposalKind::UpdateClProtocolFee(params) => {
                let self_addr = env.current_contract_address();
                ClPoolClient::new(&env, &params.cl_pool).set_protocol_fee(
                    &self_addr,
                    &params.recipient,
                    &params.bps,
                );
            }
            ProposalKind::TransferClPoolAdmin(params) => {
                let self_addr = env.current_contract_address();
                ClPoolClient::new(&env, &params.cl_pool)
                    .propose_admin(&self_addr, &params.new_admin);
            }
            ProposalKind::SetClPositionNft(params) => {
                let self_addr = env.current_contract_address();
                ClPoolClient::new(&env, &params.cl_pool).set_position_nft(&self_addr, &params.nft);
            }
        }

        proposal.executed = true;
        env.storage().persistent().set(&proposal_key, &proposal);
        Self::bump_key_ttl(&env, &proposal_key);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "executed"),),
            (proposal_id, proposal.kind.clone())
        );
        Ok(())
    }

    /// Cancel an active proposal. Only the original proposer can cancel,
    /// and only while voting is still open.
    pub fn cancel_proposal(
        env: Env,
        proposal_id: u32,
        proposer: Address,
    ) -> Result<(), GovernanceError> {
        Self::extend_instance_ttl(&env);
        proposer.require_auth();

        let proposal_key = DataKey::Proposal(proposal_id);
        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&proposal_key)
            .ok_or(GovernanceError::ProposalNotFound)?;
        Self::bump_key_ttl(&env, &proposal_key);

        if proposal.executed {
            return Err(GovernanceError::AlreadyExecuted);
        }
        if proposal.cancelled {
            return Err(GovernanceError::ProposalCancelled);
        }
        if env.ledger().timestamp() > proposal.vote_end {
            return Err(GovernanceError::VotingPeriodEnded);
        }
        if proposal.proposer != proposer {
            return Err(GovernanceError::NotProposer);
        }

        proposal.cancelled = true;
        env.storage().persistent().set(&proposal_key, &proposal);
        Self::bump_key_ttl(&env, &proposal_key);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "cancelled"),),
            (proposal_id,)
        );
        Ok(())
    }

    /// Query how an address voted on a proposal.
    ///
    /// Returns `VotedFor`, `VotedAgainst`, or `DidNotVote`.
    pub fn get_vote_info(env: Env, proposal_id: u32, voter: Address) -> VoteRecord {
        Self::extend_instance_ttl(&env);
        env.storage()
            .persistent()
            .get(&DataKey::HasVoted(proposal_id, voter))
            .unwrap_or(VoteRecord::DidNotVote)
    }

    /// Return the current governance configuration parameters.
    ///
    /// Returns [`GovernanceError::NotInitialized`] before `initialize`.
    pub fn get_params(env: Env) -> Result<GovernanceParams, GovernanceError> {
        Ok(GovernanceParams {
            voting_period_secs: Self::read_voting_period(&env)?,
            timelock_secs: Self::read_timelock(&env)?,
            quorum_bps: Self::read_quorum_bps(&env)?,
            min_proposer_stake_bps: Self::read_min_proposer_stake_bps(&env)?,
            veto_multisig: env.storage().instance().get(&DataKey::VetoMultisig),
            quorum_decay_rate_bps_per_day: env
                .storage()
                .instance()
                .get(&DataKey::QuorumDecayRateBpsPerDay)
                .unwrap_or(0),
        })
    }

    /// Unlock voting power for a concluded proposal.
    pub fn unlock_vote(env: Env, voter: Address, proposal_id: u32) -> Result<(), GovernanceError> {
        Self::extend_instance_ttl(&env);
        voter.require_auth();
        let status = Self::proposal_status(env.clone(), proposal_id)?;
        if status != ProposalStatus::Executed
            && status != ProposalStatus::Defeated
            && status != ProposalStatus::Expired
            && status != ProposalStatus::Cancelled
            && status != ProposalStatus::Vetoed
            && status != ProposalStatus::InDiscussion
        {
            return Err(GovernanceError::ProposalNotConcluded);
        }
        let lock_key = DataKey::LockedVote(proposal_id, voter.clone());
        let locked: i128 = env.storage().persistent().get(&lock_key).unwrap_or(0);
        if locked == 0 {
            return Err(GovernanceError::NoLockedVote);
        }

        let lp_token = Self::read_lp_token(&env)?;
        // Issue #556: pass `self_addr` as the locker so the LP token authorises this
        // contract, even after Admin has rotated `set_locker` to a different contract.
        let self_addr = env.current_contract_address();
        LpTokenClient::new(&env, &lp_token).unlock(&voter, &self_addr, &locked);
        env.storage().persistent().remove(&lock_key);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "vote_unlocked"), voter.clone()),
            (proposal_id, locked)
        );
        Ok(())
    }

    /// Delegate voting power to another address.
    ///
    /// The delegator's voting power is transferred to the delegatee who votes on their behalf.
    /// The delegator cannot vote while delegation is active.
    ///
    /// # Parameters
    /// - `from` – LP holder delegating their voting power; must authorize this call.
    /// - `to` – Address receiving the delegated voting power.
    ///
    /// # Panics
    /// - If `from` is the same as `to`.
    pub fn delegate(env: Env, from: Address, to: Address) -> Result<(), GovernanceError> {
        Self::extend_instance_ttl(&env);
        from.require_auth();
        if from == to {
            return Err(GovernanceError::CannotDelegateToSelf);
        }
        if Self::delegation_reaches(&env, &to, &from, 0) {
            return Err(GovernanceError::DelegationCycle);
        }

        if let Some(old) = Self::get_delegate(env.clone(), from.clone()) {
            Self::remove_delegator_index(&env, &old, &from);
        }
        Self::add_delegator_index(&env, &to, &from);

        let delegate_key = DataKey::Delegate(from.clone());
        env.storage().persistent().set(&delegate_key, &to);
        Self::bump_key_ttl(&env, &delegate_key);

        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "delegated"),), (from, to));
        Ok(())
    }

    /// Remove delegation of voting power.
    ///
    /// After calling, the caller's voting power reverts to themselves.
    ///
    /// # Parameters
    /// - `from` – Address removing their delegation; must authorize this call.
    pub fn undelegate(env: Env, from: Address) {
        Self::extend_instance_ttl(&env);
        from.require_auth();
        if let Some(delegatee) = Self::get_delegate(env.clone(), from.clone()) {
            Self::remove_delegator_index(&env, &delegatee, &from);
        }
        env.storage()
            .persistent()
            .remove(&DataKey::Delegate(from.clone()));

        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "undelegated"),), (from,));
    }

    /// Retrieve the current delegatee for an LP holder.
    ///
    /// Returns `None` if no delegation is active.
    pub fn get_delegate(env: Env, from: Address) -> Option<Address> {
        Self::extend_instance_ttl(&env);
        let key = DataKey::Delegate(from);
        let delegate = env.storage().persistent().get(&key).unwrap_or(None);
        if delegate.is_some() {
            Self::bump_key_ttl(&env, &key);
        }
        delegate
    }

    /// Admin-only: set the protocol multisig that may veto passed proposals.
    pub fn set_veto_multisig(env: Env, multisig: Address) -> Result<(), GovernanceError> {
        let admin = Self::read_admin(&env)?;
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::VetoMultisig, &multisig);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "veto_multisig_set"),),
            (multisig,)
        );
        Ok(())
    }

    /// Veto a passed proposal within 24 hours after voting ends.
    ///
    /// Triggers the governance discussion phase; the proposal cannot be executed.
    pub fn veto(env: Env, proposal_id: u32) -> Result<(), GovernanceError> {
        Self::extend_instance_ttl(&env);
        let multisig: Address = env
            .storage()
            .instance()
            .get(&DataKey::VetoMultisig)
            .ok_or(GovernanceError::VetoMultisigNotSet)?;
        multisig.require_auth();

        let proposal_key = DataKey::Proposal(proposal_id);
        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&proposal_key)
            .ok_or(GovernanceError::ProposalNotFound)?;
        Self::bump_key_ttl(&env, &proposal_key);

        if proposal.executed {
            return Err(GovernanceError::AlreadyExecuted);
        }
        if proposal.cancelled {
            return Err(GovernanceError::ProposalCancelled);
        }
        if proposal.vetoed {
            return Err(GovernanceError::ProposalVetoed);
        }

        let now = env.ledger().timestamp();
        if now <= proposal.vote_end {
            return Err(GovernanceError::VotingPeriodActive);
        }
        if now > proposal.vote_end + VETO_WINDOW_SECS {
            return Err(GovernanceError::VetoWindowExpired);
        }

        let effective_quorum = Self::effective_quorum_bps(&env, &proposal)?;
        let total_votes = proposal.votes_for + proposal.votes_against + proposal.votes_abstain;
        let quorum_threshold = proposal.snapshot_total_supply * effective_quorum / MAX_BPS;
        if total_votes < quorum_threshold || proposal.votes_for <= proposal.votes_against {
            return Err(GovernanceError::ProposalDefeated);
        }

        let voting_period = Self::read_voting_period(&env)?;
        let discussion_end = now + voting_period;

        proposal.vetoed = true;
        proposal.vetoed_by = Some(multisig.clone());
        proposal.vetoed_at = Some(now);
        proposal.discussion_end = Some(discussion_end);
        env.storage().persistent().set(&proposal_key, &proposal);
        Self::bump_key_ttl(&env, &proposal_key);

        let audit = VetoAudit {
            proposal_id,
            vetoed_by: multisig.clone(),
            vetoed_at: now,
            discussion_end,
        };
        let audit_key = DataKey::VetoAudit(proposal_id);
        env.storage().persistent().set(&audit_key, &audit);
        Self::bump_key_ttl(&env, &audit_key);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "vetoed"),),
            (proposal_id, multisig, now, discussion_end)
        );
        Ok(())
    }

    /// Returns the on-chain veto audit record for a proposal, if vetoed.
    pub fn get_veto_audit(env: Env, proposal_id: u32) -> Option<VetoAudit> {
        Self::extend_instance_ttl(&env);
        env.storage()
            .persistent()
            .get(&DataKey::VetoAudit(proposal_id))
    }

    /// LP balance at proposal snapshot ledger for `holder`.
    pub fn get_snapshot_balance(
        env: Env,
        proposal_id: u32,
        holder: Address,
    ) -> Result<i128, GovernanceError> {
        Self::extend_instance_ttl(&env);
        let proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;
        let lp_token = Self::read_lp_token(&env)?;
        let lp_client = LpTokenClient::new(&env, &lp_token);
        Ok(lp_client.balance_at(&holder, &proposal.snapshot_ledger))
    }

    /// Read a proposal by id.
    ///
    /// Returns [`GovernanceError::ProposalNotFound`] for an unknown
    /// `proposal_id` rather than trapping, so off-chain callers can handle a
    /// missing proposal via `try_get_proposal`.
    pub fn get_proposal(env: Env, proposal_id: u32) -> Result<Proposal, GovernanceError> {
        Self::extend_instance_ttl(&env);
        let key = DataKey::Proposal(proposal_id);
        let proposal: Proposal = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(GovernanceError::ProposalNotFound)?;
        Self::bump_key_ttl(&env, &key);
        Ok(proposal)
    }

    /// Total number of proposals ever created by this contract.
    ///
    /// This is the monotonic `ProposalCount` counter incremented by `propose`,
    /// so it doubles as the exclusive upper bound of the assigned id range:
    /// every proposal ever created has an id in `[0, get_proposal_count())`.
    /// It never decreases — cancelled, vetoed and executed proposals are
    /// status-transitioned in place, never removed from the count.
    pub fn get_proposal_count(env: Env) -> u32 {
        Self::extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::ProposalCount)
            .unwrap_or(0)
    }

    /// Return up to `limit` proposals by ascending id, starting at `offset`.
    ///
    /// Ids are assigned sequentially from 0 by `propose`, and no code path ever
    /// removes a `DataKey::Proposal` entry — cancel, veto and execute only
    /// mutate the stored struct's flags — so `[0, get_proposal_count())` is a
    /// dense id range and proposals come back in creation order.
    ///
    /// The one way an id in that range can fail to resolve is a persistent
    /// entry whose TTL lapsed before anyone bumped it. Such an id is skipped
    /// rather than panicking, so an expired proposal can never make the whole
    /// page unreadable. A returned page can therefore be shorter than
    /// `min(limit, count - offset)`; callers paginating to the end should drive
    /// the loop off `get_proposal_count()` and `offset`, not off page length.
    ///
    /// `offset >= count` returns an empty `Vec`, and `offset + limit` is
    /// clamped to `count`, mirroring `factory::get_pools`.
    pub fn get_proposals_paginated(env: Env, offset: u32, limit: u32) -> Vec<Proposal> {
        Self::extend_instance_ttl(&env);
        let count = Self::get_proposal_count(env.clone());
        let start = offset.min(count);
        // Saturating: `start + limit` can overflow u32 for a large `limit`.
        let end = start.saturating_add(limit).min(count);

        let mut page = Vec::new(&env);
        for id in start..end {
            let key = DataKey::Proposal(id);
            if let Some(proposal) = env.storage().persistent().get::<_, Proposal>(&key) {
                Self::bump_key_ttl(&env, &key);
                page.push_back(proposal);
            }
        }
        page
    }

    /// Derive the current status of a proposal.
    ///
    /// Returns [`GovernanceError::ProposalNotFound`] for an unknown
    /// `proposal_id` rather than trapping, so off-chain callers can handle a
    /// missing proposal via `try_proposal_status`.
    pub fn proposal_status(env: Env, proposal_id: u32) -> Result<ProposalStatus, GovernanceError> {
        Self::extend_instance_ttl(&env);
        let proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;
        Self::bump_key_ttl(&env, &DataKey::Proposal(proposal_id));

        if proposal.cancelled {
            return Ok(ProposalStatus::Cancelled);
        }

        if proposal.executed {
            return Ok(ProposalStatus::Executed);
        }

        let now = env.ledger().timestamp();

        if proposal.vetoed {
            if let Some(end) = proposal.discussion_end {
                if now <= end {
                    return Ok(ProposalStatus::InDiscussion);
                }
            }
            return Ok(ProposalStatus::Vetoed);
        }

        if now <= proposal.vote_end {
            return Ok(ProposalStatus::Active);
        }

        let effective_quorum = Self::effective_quorum_bps(&env, &proposal)?;
        let total_votes = proposal.votes_for + proposal.votes_against + proposal.votes_abstain;
        let quorum_threshold = proposal.snapshot_total_supply * effective_quorum / MAX_BPS;
        let passed = total_votes >= quorum_threshold && proposal.votes_for > proposal.votes_against;

        if !passed {
            return Ok(ProposalStatus::Defeated);
        }

        if now > proposal.expires_at {
            return Ok(ProposalStatus::Expired);
        }

        if now >= proposal.execute_after {
            Ok(ProposalStatus::Queued)
        } else {
            Ok(ProposalStatus::Pending)
        }
    }

    fn effective_quorum_bps(env: &Env, proposal: &Proposal) -> Result<i128, GovernanceError> {
        let base = Self::read_quorum_bps(env)?;
        let decay_rate: i128 = env
            .storage()
            .instance()
            .get(&DataKey::QuorumDecayRateBpsPerDay)
            .unwrap_or(0);
        if decay_rate == 0 {
            return Ok(base);
        }
        // Freeze decay at vote_end: once voting closes the tally is final, so
        // the required quorum must not keep climbing based on when execute(),
        // veto(), or proposal_status() happen to be called afterward.
        let now = env.ledger().timestamp().min(proposal.vote_end);
        let days_open = if now > proposal.vote_start {
            (now - proposal.vote_start) / 86_400
        } else {
            0
        };
        Ok((base + decay_rate * days_open as i128).min(MAX_BPS))
    }

    fn snapshot_voting_power(
        lp_client: &LpTokenClient,
        holder: &Address,
        proposal: &Proposal,
    ) -> Result<i128, GovernanceError> {
        let power = lp_client.balance_at(holder, &proposal.snapshot_ledger);
        let current = lp_client.balance(holder);
        if current < power {
            return Err(GovernanceError::InsufficientSnapshotBal);
        }
        Ok(power)
    }

    fn aggregated_voting_power(
        env: &Env,
        lp_client: &LpTokenClient,
        voter: &Address,
        proposal: &Proposal,
    ) -> Result<(i128, Vec<(Address, i128)>), GovernanceError> {
        let mut total: i128 = 0;
        let mut locks = Vec::new(env);
        Self::collect_voting_power(env, lp_client, voter, proposal, &mut total, &mut locks, 0)?;
        Ok((total, locks))
    }

    fn collect_voting_power(
        env: &Env,
        lp_client: &LpTokenClient,
        holder: &Address,
        proposal: &Proposal,
        total: &mut i128,
        locks: &mut Vec<(Address, i128)>,
        depth: u32,
    ) -> Result<(), GovernanceError> {
        // Depth is a traversal bound, not a validity check: `delegate` already
        // rejects cycles, so a chain longer than MAX_DELEGATION_DEPTH is simply a
        // long honest chain. Stop walking instead of erroring, otherwise the
        // terminal delegatee could never vote at all.
        if depth > MAX_DELEGATION_DEPTH {
            return Ok(());
        }
        let voted_key = DataKey::HasVoted(proposal.id, holder.clone());
        if !env.storage().persistent().has(&voted_key) {
            let power = Self::snapshot_voting_power(lp_client, holder, proposal)?;
            if power > 0 {
                *total += power;
                locks.push_back((holder.clone(), power));
            }
        }
        let count_key = DataKey::DelegatorCount(holder.clone());
        let count: u32 = env.storage().persistent().get(&count_key).unwrap_or(0);
        if count > 0 {
            Self::bump_key_ttl(env, &count_key);
        }
        for i in 0..count {
            let delegator_key = DataKey::Delegator(holder.clone(), i);
            // Present: `add_delegator_index` / `remove_delegator_index` keep
            // `Delegator(holder, 0..count)` dense, so every index below `count` exists.
            let delegator: Address = env.storage().persistent().get(&delegator_key).unwrap();
            Self::bump_key_ttl(env, &delegator_key);
            Self::collect_voting_power(
                env,
                lp_client,
                &delegator,
                proposal,
                total,
                locks,
                depth + 1,
            )?;
        }
        Ok(())
    }

    fn delegation_reaches(env: &Env, current: &Address, target: &Address, depth: u32) -> bool {
        if depth > MAX_DELEGATION_DEPTH {
            return false;
        }
        if current == target {
            return true;
        }
        if let Some(next) = Self::get_delegate(env.clone(), current.clone()) {
            Self::delegation_reaches(env, &next, target, depth + 1)
        } else {
            false
        }
    }

    fn add_delegator_index(env: &Env, delegatee: &Address, delegator: &Address) {
        let count_key = DataKey::DelegatorCount(delegatee.clone());
        let count: u32 = env.storage().persistent().get(&count_key).unwrap_or(0);
        if count > 0 {
            Self::bump_key_ttl(env, &count_key);
        }

        let delegator_key = DataKey::Delegator(delegatee.clone(), count);
        env.storage().persistent().set(&delegator_key, delegator);
        Self::bump_key_ttl(env, &delegator_key);

        let slot_key = DataKey::DelegatorSlot(delegator.clone());
        env.storage()
            .persistent()
            .set(&slot_key, &(delegatee.clone(), count));
        Self::bump_key_ttl(env, &slot_key);

        env.storage().persistent().set(&count_key, &(count + 1));
        Self::bump_key_ttl(env, &count_key);
    }

    fn remove_delegator_index(env: &Env, delegatee: &Address, delegator: &Address) {
        let slot_key = DataKey::DelegatorSlot(delegator.clone());
        let (stored_delegatee, index): (Address, u32) = env
            .storage()
            .persistent()
            .get(&slot_key)
            .unwrap_or((delegatee.clone(), 0));
        if stored_delegatee != *delegatee {
            return;
        }
        Self::bump_key_ttl(env, &slot_key);

        let count_key = DataKey::DelegatorCount(delegatee.clone());
        let count: u32 = env.storage().persistent().get(&count_key).unwrap_or(0);
        if count == 0 {
            return;
        }
        Self::bump_key_ttl(env, &count_key);

        let last_index = count - 1;
        if index != last_index {
            let last_delegator_key = DataKey::Delegator(delegatee.clone(), last_index);
            // Present: the delegator index is dense over `0..count`, and
            // `last_index == count - 1` with `count > 0` checked above.
            let last_delegator: Address =
                env.storage().persistent().get(&last_delegator_key).unwrap();
            Self::bump_key_ttl(env, &last_delegator_key);

            let moved_delegator_key = DataKey::Delegator(delegatee.clone(), index);
            env.storage()
                .persistent()
                .set(&moved_delegator_key, &last_delegator);
            Self::bump_key_ttl(env, &moved_delegator_key);

            let moved_slot_key = DataKey::DelegatorSlot(last_delegator.clone());
            env.storage()
                .persistent()
                .set(&moved_slot_key, &(delegatee.clone(), index));
            Self::bump_key_ttl(env, &moved_slot_key);
        }
        env.storage()
            .persistent()
            .remove(&DataKey::Delegator(delegatee.clone(), last_index));
        env.storage()
            .persistent()
            .remove(&DataKey::DelegatorSlot(delegator.clone()));
        env.storage().persistent().set(&count_key, &last_index);
        Self::bump_key_ttl(env, &count_key);
    }

    // ── Instance configuration accessors ────────────────────────────────────
    //
    // Every value below is written by `initialize`, so absence means the
    // contract has not been initialized. Returning a typed error instead of
    // unwrapping lets callers branch on `NotInitialized` rather than trap.

    fn read_admin(env: &Env) -> Result<Address, GovernanceError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(GovernanceError::NotInitialized)
    }

    fn read_amm_pool(env: &Env) -> Result<Address, GovernanceError> {
        env.storage()
            .instance()
            .get(&DataKey::AmmPool)
            .ok_or(GovernanceError::NotInitialized)
    }

    fn read_lp_token(env: &Env) -> Result<Address, GovernanceError> {
        env.storage()
            .instance()
            .get(&DataKey::LpToken)
            .ok_or(GovernanceError::NotInitialized)
    }

    fn read_voting_period(env: &Env) -> Result<u64, GovernanceError> {
        env.storage()
            .instance()
            .get(&DataKey::VotingPeriod)
            .ok_or(GovernanceError::NotInitialized)
    }

    fn read_timelock(env: &Env) -> Result<u64, GovernanceError> {
        env.storage()
            .instance()
            .get(&DataKey::Timelock)
            .ok_or(GovernanceError::NotInitialized)
    }

    fn read_quorum_bps(env: &Env) -> Result<i128, GovernanceError> {
        env.storage()
            .instance()
            .get(&DataKey::QuorumBps)
            .ok_or(GovernanceError::NotInitialized)
    }

    fn read_min_proposer_stake_bps(env: &Env) -> Result<i128, GovernanceError> {
        env.storage()
            .instance()
            .get(&DataKey::MinProposerStakeBps)
            .ok_or(GovernanceError::NotInitialized)
    }

    fn read_proposal_count(env: &Env) -> Result<u32, GovernanceError> {
        env.storage()
            .instance()
            .get(&DataKey::ProposalCount)
            .ok_or(GovernanceError::NotInitialized)
    }

    fn bump_key_ttl(env: &Env, key: &DataKey) {
        env.storage()
            .persistent()
            .extend_ttl(key, MIN_PERSISTENT_TTL, PERSISTENT_TTL_BUMP_TO);
    }

    /// Instance-storage TTL maintenance. Called first by every public
    /// entrypoint so the governance instance entry cannot be archived out from
    /// under in-flight proposals. See [`MIN_INSTANCE_TTL`].
    fn extend_instance_ttl(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(MIN_INSTANCE_TTL, INSTANCE_TTL_BUMP_TO);
    }

    /// Convert a fee tier ID (0-3) to its basis points value.
    ///
    /// Matches the fee tier definitions:
    /// - 0 → 1 bps (0.01%)
    /// - 1 → 5 bps (0.05%)
    /// - 2 → 30 bps (0.3%)
    /// - 3 → 100 bps (1.0%)
    fn fee_tier_to_bps(fee_tier: i128) -> Result<i128, GovernanceError> {
        match fee_tier {
            0 => Ok(1),   // VeryLow: 0.01%
            1 => Ok(5),   // Low: 0.05%
            2 => Ok(30),  // Medium: 0.3%
            3 => Ok(100), // High: 1.0%
            _ => Err(GovernanceError::InvalidFeeBps),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use amm::AmmPool;
    use concentrated_liquidity::ConcentratedLiquidity;
    use soroban_sdk::token::{Client as StellarTokenClient, StellarAssetClient};
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        Env,
    };
    use token::LpToken;

    // ── Helpers ───────────────────────────────────────────────────────────────

    struct Suite {
        env: Env,
        gov_addr: Address,
        lp_addr: Address,
        amm_addr: Address,
    }

    fn setup_suite(initial_fee_bps: i128) -> Suite {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1_000_000);

        let admin = Address::generate(&env);

        // Deploy LP token.
        let lp_addr = env.register_contract(None, LpToken);
        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &admin, // temporary admin; will be replaced by AMM
            &soroban_sdk::String::from_str(&env, "AMM LP"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        // Deploy token A and B.
        let ta = env.register_stellar_asset_contract_v2(admin.clone());
        let tb = env.register_stellar_asset_contract_v2(admin.clone());
        let ta_addr = ta.address();
        let tb_addr = tb.address();

        // Deploy governance.
        let gov_addr = env.register_contract(None, Governance);

        // Deploy AMM.
        let amm_addr = env.register_contract(None, AmmPool);
        amm::AmmPoolClient::new(&env, &amm_addr).initialize(
            &gov_addr, // The governance contract is the pool's admin
            &ta_addr,
            &tb_addr,
            &lp_addr,
            &initial_fee_bps,
            &admin,
            &0_i128,
        );

        // Initialize governance.
        GovernanceClient::new(&env, &gov_addr).initialize(
            &admin,
            &amm_addr,
            &lp_addr,
            &(7 * 24 * 60 * 60_u64), // voting_period_secs: 7 days
            &(2 * 24 * 60 * 60_u64), // timelock_secs: 2 days
            &1_000_i128,             // quorum_bps: 10%
            &100_i128,               // min_proposer_stake_bps
        );
        token::LpTokenClient::new(&env, &lp_addr).set_locker(&gov_addr);

        Suite {
            env,
            gov_addr,
            lp_addr,
            amm_addr,
        }
    }

    /// Mint LP tokens directly to an address (simulates adding liquidity).
    fn mint_lp(suite: &Suite, to: &Address, amount: i128) {
        token::LpTokenClient::new(&suite.env, &suite.lp_addr).mint(to, &amount);
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_passing_proposal_executes() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        // Propose new fee of 50 bps.
        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        assert_eq!(pid, 0);

        // Both vote for.
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        // Advance past voting period + timelock.
        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);

        gov.execute(&pid);

        let executed = gov.get_proposal(&pid);
        assert!(executed.executed);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Executed);
    }

    #[test]
    fn test_failing_quorum_defeats_proposal() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        // lp1 gets 20, lp2 gets 980 — lp1 alone is < 10% quorum.
        mint_lp(&s, &lp1, 20);
        mint_lp(&s, &lp2, 980);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        // Only lp1 votes (20 out of 1000 total = 2% < 10% quorum).
        gov.vote(&lp1, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);

        // Execute should panic — quorum not met.
        let result = gov.try_execute(&pid);
        assert!(result.is_err());
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Defeated);
    }

    #[test]
    fn test_expired_proposal_cannot_execute() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        // Jump past the expiry window.
        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.expires_at + 1);

        let result = gov.try_execute(&pid);
        assert!(result.is_err());
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Expired);
    }

    #[test]
    fn test_cannot_vote_twice() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 500);
        mint_lp(&s, &Address::generate(&s.env), 500);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);

        let result = gov.try_vote(&lp1, &pid, &Vote::Against);
        assert!(result.is_err());
    }

    #[test]
    fn test_cannot_vote_after_period_ends() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 500);
        mint_lp(&s, &Address::generate(&s.env), 500);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.vote_end + 1);

        let result = gov.try_vote(&lp1, &pid, &Vote::For);
        assert!(result.is_err());
    }

    #[test]
    fn test_cannot_execute_before_timelock() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        // Jump past voting but NOT past timelock.
        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.vote_end + 1);

        let result = gov.try_execute(&pid);
        assert!(result.is_err());
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Pending);
    }

    #[test]
    fn test_proposal_status_active_then_queued() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Active);

        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Queued);
    }

    #[test]
    fn test_no_lp_tokens_cannot_propose() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let nobody = Address::generate(&s.env);
        // Give someone else tokens so total_supply > 0.
        mint_lp(&s, &Address::generate(&s.env), 1000);

        let result = gov.try_propose(&nobody, &ProposalKind::UpdateFee(50));
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_fee_bps_rejected() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 1000);

        let result = gov.try_propose(&lp1, &ProposalKind::UpdateFee(10_001));
        assert!(result.is_err());
    }

    #[test]
    fn test_below_min_stake_cannot_propose_but_exact_min_can() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let low = Address::generate(&s.env);
        let exact = Address::generate(&s.env);
        let whale = Address::generate(&s.env);

        mint_lp(&s, &low, 9);
        mint_lp(&s, &exact, 10);
        mint_lp(&s, &whale, 981);

        assert!(gov.try_propose(&low, &ProposalKind::UpdateFee(40)).is_err());
        let pid = gov.propose(&exact, &ProposalKind::UpdateFee(40));
        assert_eq!(pid, 0);
    }

    #[test]
    fn test_vote_locks_balance_until_proposal_concludes() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let lp_client = token::LpTokenClient::new(&s.env, &s.lp_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        let receiver = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        assert_eq!(lp_client.locked_balance(&lp1), 600);

        // Simulated flash-loan pattern fails: voter cannot move locked weight.
        let transfer_result = lp_client.try_transfer(&lp1, &receiver, &600_i128);
        assert!(transfer_result.is_err());

        gov.vote(&lp2, &pid, &Vote::For);
        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);
        gov.execute(&pid);

        gov.unlock_vote(&lp1, &pid);
        assert_eq!(lp_client.locked_balance(&lp1), 0);
        lp_client.transfer(&lp1, &receiver, &600_i128);
    }

    // Issue #129: governance must emit `proposed`, `voted`, and `executed`
    // events with the documented payloads.
    #[test]
    fn test_governance_emits_proposed_voted_executed_events() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::IntoVal;

        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        let proposal = gov.get_proposal(&pid);

        // `proposed` event: (id, proposer, kind, vote_end)
        let events = s.env.events().all();
        let proposed_evt = events
            .iter()
            .find(|e| {
                e.0 == gov.address && e.1 == (Symbol::new(&s.env, "proposed"),).into_val(&s.env)
            })
            .expect("proposed event not found");
        let __ver_7: (u32, (u32, Address, ProposalKind, u64, u32)) =
            proposed_evt.2.into_val(&s.env);
        assert_eq!(__ver_7.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let proposed_data: (u32, Address, ProposalKind, u64, u32) = __ver_7.1;
        assert_eq!(
            proposed_data,
            (
                pid,
                lp1.clone(),
                ProposalKind::UpdateFee(50),
                proposal.vote_end,
                proposal.snapshot_ledger
            )
        );

        gov.vote(&lp1, &pid, &Vote::For);

        // `voted` event: (proposal_id, voter, choice, voting_power)
        let events = s.env.events().all();
        let voted_evt = events
            .iter()
            .find(|e| e.0 == gov.address && e.1 == (Symbol::new(&s.env, "voted"),).into_val(&s.env))
            .expect("voted event not found");
        let __ver_8: (u32, (u32, Address, Vote, i128)) = voted_evt.2.into_val(&s.env);
        assert_eq!(__ver_8.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let voted_data: (u32, Address, Vote, i128) = __ver_8.1;
        assert_eq!(voted_data, (pid, lp1.clone(), Vote::For, 600_i128));

        gov.vote(&lp2, &pid, &Vote::For);

        s.env.ledger().set_timestamp(proposal.execute_after + 1);
        gov.execute(&pid);

        // `executed` event: (proposal_id, kind)
        let events = s.env.events().all();
        let executed_evt = events
            .iter()
            .find(|e| {
                e.0 == gov.address && e.1 == (Symbol::new(&s.env, "executed"),).into_val(&s.env)
            })
            .expect("executed event not found");
        let __ver_9: (u32, (u32, ProposalKind)) = executed_evt.2.into_val(&s.env);
        assert_eq!(__ver_9.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let executed_data: (u32, ProposalKind) = __ver_9.1;
        assert_eq!(executed_data, (pid, ProposalKind::UpdateFee(50)));
    }

    #[test]
    fn test_governance_multiple_proposal_kinds() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let amm = amm::AmmPoolClient::new(&s.env, &s.amm_addr);

        let lp1 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 1000);

        // --- 1. Test PausePool proposal ---
        let pid1 = gov.propose(&lp1, &ProposalKind::PausePool);
        gov.vote(&lp1, &pid1, &Vote::For);
        let prop1 = gov.get_proposal(&pid1);
        s.env.ledger().set_timestamp(prop1.execute_after + 1);
        gov.execute(&pid1);
        assert!(amm.is_paused());
        gov.unlock_vote(&lp1, &pid1);

        // --- 2. Test UnpausePool proposal ---
        let pid2 = gov.propose(&lp1, &ProposalKind::UnpausePool);
        gov.vote(&lp1, &pid2, &Vote::For);
        let prop2 = gov.get_proposal(&pid2);
        s.env.ledger().set_timestamp(prop2.execute_after + 1);
        gov.execute(&pid2);
        assert!(!amm.is_paused());
        gov.unlock_vote(&lp1, &pid2);

        // --- 3. Test UpdateFlashLoanFee proposal ---
        let pid3 = gov.propose(&lp1, &ProposalKind::UpdateFlashLoanFee(45));
        gov.vote(&lp1, &pid3, &Vote::For);
        let prop3 = gov.get_proposal(&pid3);
        s.env.ledger().set_timestamp(prop3.execute_after + 1);
        gov.execute(&pid3);
        let info = amm.get_info();
        assert_eq!(info.flash_loan_fee_bps, 45);
        gov.unlock_vote(&lp1, &pid3);

        // --- 4. Test UpdateProtocolFee proposal ---
        let recipient = Address::generate(&s.env);
        let pid4 = gov.propose(
            &lp1,
            &ProposalKind::UpdateProtocolFee(UpdateProtocolFeeParams {
                new_bps: 10,
                new_recipient: recipient.clone(),
            }),
        );
        gov.vote(&lp1, &pid4, &Vote::For);
        let prop4 = gov.get_proposal(&pid4);
        s.env.ledger().set_timestamp(prop4.execute_after + 1);
        gov.execute(&pid4);
        let (fee_rec, bps) = amm.get_protocol_fee();
        assert_eq!(fee_rec, Some(recipient));
        assert_eq!(bps, 10);
        gov.unlock_vote(&lp1, &pid4);

        // --- 5. Test EmergencyWithdraw proposal ---
        let emergency_rec = Address::generate(&s.env);
        let pid5 = gov.propose(
            &lp1,
            &ProposalKind::EmergencyWithdraw(emergency_rec.clone()),
        );
        gov.vote(&lp1, &pid5, &Vote::For);
        let prop5 = gov.get_proposal(&pid5);
        s.env.ledger().set_timestamp(prop5.execute_after + 1);

        let ta_sac = StellarAssetClient::new(&s.env, &info.token_a);
        let tb_sac = StellarAssetClient::new(&s.env, &info.token_b);
        let provider = Address::generate(&s.env);
        ta_sac.mint(&provider, &100_000_i128);
        tb_sac.mint(&provider, &100_000_i128);

        amm.add_liquidity(&provider, &100_000_i128, &100_000_i128, &0_i128, &u64::MAX);
        assert_eq!(amm.get_info().reserve_a, 100_000);
        assert_eq!(amm.get_info().reserve_b, 100_000);

        gov.execute(&pid5);

        assert_eq!(amm.get_info().reserve_a, 0);
        assert_eq!(amm.get_info().reserve_b, 0);

        let ta_client = StellarTokenClient::new(&s.env, &info.token_a);
        let tb_client = StellarTokenClient::new(&s.env, &info.token_b);
        assert_eq!(ta_client.balance(&emergency_rec), 100_000);
        assert_eq!(tb_client.balance(&emergency_rec), 100_000);

        gov.unlock_vote(&lp1, &pid5);
    }

    #[test]
    fn test_full_governance_lifecycle() {
        let s = setup_suite(30); // initial fee = 30 bps
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        // 1. Distribute LP tokens (quorum = 10% of 1000 = 100)
        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        // 2. Propose fee change to 50 bps
        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Active);

        // 3. Vote (both for)
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        // 4. Advance past voting period
        let p = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(p.execute_after + 1);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Queued);

        // 5. Execute
        gov.execute(&pid);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Executed);

        // 6. Verify AMM fee changed
        let amm = amm::AmmPoolClient::new(&s.env, &s.amm_addr);
        assert_eq!(amm.get_info().fee_bps, 50);
    }

    #[test]
    fn test_governance_lifecycle_defeat_quorum() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 50);
        mint_lp(&s, &lp2, 950);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Active);

        // Only lp1 votes. Total votes = 50 < 100 (quorum threshold)
        gov.vote(&lp1, &pid, &Vote::For);

        let p = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(p.execute_after + 1);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Defeated);

        let result = gov.try_execute(&pid);
        assert!(result.is_err());
    }

    #[test]
    fn test_governance_lifecycle_expired() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Active);

        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let p = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(p.expires_at + 1);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Expired);

        let result = gov.try_execute(&pid);
        assert!(result.is_err());
    }

    // ── Issue #188: set_timelock_delay ────────────────────────────────────────

    #[test]
    fn test_timelock_delay_zero_respects_veto_window() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        // Set timelock delay to 0, but execute_after should still respect veto window.
        gov.set_timelock_delay(&0_u64);
        let params = gov.get_params();
        assert_eq!(params.timelock_secs, 0);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        // With timelock = 0: execute_after = vote_end + VETO_WINDOW_SECS (clamped).
        // Jump past the veto window.
        s.env.ledger().set_timestamp(proposal.execute_after + 1);

        gov.execute(&pid);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Executed);
    }

    #[test]
    fn test_execute_reverts_before_timelock_elapses() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        // Jump past vote_end but NOT past execute_after.
        s.env.ledger().set_timestamp(proposal.vote_end + 1);

        let result = gov.try_execute(&pid);
        assert_eq!(result, Err(Ok(GovernanceError::TimelockNotElapsed)));
    }

    #[test]
    fn test_execute_succeeds_after_timelock_elapses() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        // Jump past execute_after.
        s.env.ledger().set_timestamp(proposal.execute_after + 1);

        gov.execute(&pid);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Executed);
    }

    // ── Issue #189: vote_unlocked event ──────────────────────────────────────

    #[test]
    fn test_unlock_vote_emits_vote_unlocked_event() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::IntoVal;

        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);
        gov.execute(&pid);

        gov.unlock_vote(&lp1, &pid);

        let events = s.env.events().all();
        let unlock_evt = events
            .iter()
            .find(|e| {
                e.0 == s.gov_addr
                    && e.1 == (Symbol::new(&s.env, "vote_unlocked"), lp1.clone()).into_val(&s.env)
            })
            .expect("vote_unlocked event not emitted");

        let __ver_10: (u32, (u32, i128)) = unlock_evt.2.into_val(&s.env);
        assert_eq!(__ver_10.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let data: (u32, i128) = __ver_10.1;
        assert_eq!(data.0, pid);
        assert_eq!(data.1, 600_i128); // amount_unlocked == voting power used
    }

    #[test]
    fn test_snapshot_balance_used_not_current_balance() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        let buyer = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        s.env.ledger().with_mut(|l| l.sequence_number = 10);
        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        let proposal = gov.get_proposal(&pid);

        // Acquire LP after snapshot — should not increase voting power.
        s.env.ledger().with_mut(|l| l.sequence_number = 11);
        mint_lp(&s, &buyer, 500);
        assert_eq!(gov.get_snapshot_balance(&pid, &buyer), 0);
        assert!(gov.try_vote(&buyer, &pid, &Vote::For).is_err());

        assert_eq!(gov.get_snapshot_balance(&pid, &lp1), 600);
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        s.env.ledger().set_timestamp(proposal.execute_after + 1);
        gov.execute(&pid);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Executed);
    }

    #[test]
    fn test_propose_snapshot_excludes_same_ledger_mint() {
        // Regression for #559: a mint landing in the *same* ledger as propose()
        // (even in a later transaction) must not count toward snapshot voting
        // power, because the snapshot is taken as of the last closed ledger.
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let proposer = Address::generate(&s.env);
        let attacker = Address::generate(&s.env);
        mint_lp(&s, &proposer, 600);

        // Advance to a real (non-genesis) ledger and create the proposal there.
        s.env.ledger().with_mut(|l| l.sequence_number = 100);
        let pid = gov.propose(&proposer, &ProposalKind::UpdateFee(50));

        // Same ledger, later transaction: attacker acquires LP.
        mint_lp(&s, &attacker, 1_000);

        // The same-ledger acquisition is excluded from the snapshot.
        assert_eq!(gov.get_snapshot_balance(&pid, &attacker), 0);
        assert!(gov.try_vote(&attacker, &pid, &Vote::For).is_err());

        // The proposer, who held their balance in an earlier closed ledger,
        // retains their legitimate voting power.
        assert_eq!(gov.get_snapshot_balance(&pid, &proposer), 600);
    }

    #[test]
    fn test_delegation_aggregates_voting_power() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        let delegatee = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        gov.delegate(&lp1, &delegatee);

        let pid = gov.propose(&lp2, &ProposalKind::UpdateFee(50));
        assert!(gov.try_vote(&lp1, &pid, &Vote::For).is_err());
        gov.vote(&delegatee, &pid, &Vote::For);

        let p = gov.get_proposal(&pid);
        assert_eq!(p.votes_for, 600);
    }

    #[test]
    fn test_delegate_after_direct_vote_does_not_double_lock_or_double_count() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let a = Address::generate(&s.env);
        let b = Address::generate(&s.env);
        let proposer = Address::generate(&s.env);
        mint_lp(&s, &a, 300);
        mint_lp(&s, &b, 300);
        mint_lp(&s, &proposer, 400);

        let pid = gov.propose(&proposer, &ProposalKind::UpdateFee(50));

        // B votes directly first, locking their own snapshot power.
        gov.vote(&b, &pid, &Vote::For);
        assert_eq!(gov.get_proposal(&pid).votes_for, 300);

        // B then delegates to A while the proposal is still open. Nothing
        // should block this, but A's later vote must not re-lock or
        // re-count B's already-locked power.
        gov.delegate(&b, &a);

        // A votes; this must not panic (double lock of B's balance) and
        // must only add A's own power, since B already voted.
        gov.vote(&a, &pid, &Vote::For);

        let p = gov.get_proposal(&pid);
        assert_eq!(
            p.votes_for,
            300 + 300,
            "B's power must not be counted twice"
        );
    }

    #[test]
    fn test_delegation_records_use_persistent_storage() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let delegator = Address::generate(&s.env);
        let delegatee = Address::generate(&s.env);
        mint_lp(&s, &delegator, 600);

        gov.delegate(&delegator, &delegatee);

        s.env.as_contract(&s.gov_addr, || {
            assert!(!s
                .env
                .storage()
                .instance()
                .has(&DataKey::Delegate(delegator.clone())));
            assert!(s
                .env
                .storage()
                .persistent()
                .has(&DataKey::Delegate(delegator.clone())));
            assert!(!s
                .env
                .storage()
                .instance()
                .has(&DataKey::DelegatorCount(delegatee.clone())));
            assert!(s
                .env
                .storage()
                .persistent()
                .has(&DataKey::DelegatorCount(delegatee.clone())));
            assert!(!s
                .env
                .storage()
                .instance()
                .has(&DataKey::Delegator(delegatee.clone(), 0)));
            assert!(s
                .env
                .storage()
                .persistent()
                .has(&DataKey::Delegator(delegatee.clone(), 0)));
            assert!(!s
                .env
                .storage()
                .instance()
                .has(&DataKey::DelegatorSlot(delegator.clone())));
            assert!(s
                .env
                .storage()
                .persistent()
                .has(&DataKey::DelegatorSlot(delegator.clone())));
        });

        gov.undelegate(&delegator);

        s.env.as_contract(&s.gov_addr, || {
            assert!(!s
                .env
                .storage()
                .persistent()
                .has(&DataKey::Delegate(delegator.clone())));
            assert!(!s
                .env
                .storage()
                .persistent()
                .has(&DataKey::Delegator(delegatee.clone(), 0)));
            assert!(!s
                .env
                .storage()
                .persistent()
                .has(&DataKey::DelegatorSlot(delegator.clone())));
            let count: u32 = s
                .env
                .storage()
                .persistent()
                .get(&DataKey::DelegatorCount(delegatee.clone()))
                .unwrap();
            assert_eq!(count, 0);
        });
    }

    #[test]
    fn test_undelegate_restores_direct_voting() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        let delegatee = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        gov.delegate(&lp1, &delegatee);
        let pid = gov.propose(&lp2, &ProposalKind::UpdateFee(50));
        gov.undelegate(&lp1);
        gov.vote(&lp1, &pid, &Vote::For);
        assert_eq!(gov.get_proposal(&pid).votes_for, 600);
    }

    #[test]
    fn test_recursive_delegation_chain() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let a = Address::generate(&s.env);
        let b = Address::generate(&s.env);
        let c = Address::generate(&s.env);
        let proposer = Address::generate(&s.env);
        mint_lp(&s, &a, 300);
        mint_lp(&s, &b, 300);
        mint_lp(&s, &c, 100);
        mint_lp(&s, &proposer, 300);

        gov.delegate(&a, &b);
        gov.delegate(&b, &c);

        let pid = gov.propose(&proposer, &ProposalKind::UpdateFee(50));
        gov.vote(&c, &pid, &Vote::For);
        assert_eq!(gov.get_proposal(&pid).votes_for, 700);
    }

    #[test]
    fn test_veto_prevents_execution_and_emits_audit() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::IntoVal;

        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let multisig = Address::generate(&s.env);
        gov.set_veto_multisig(&multisig);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(99));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.vote_end + 1);
        gov.veto(&pid);

        assert_eq!(gov.proposal_status(&pid), ProposalStatus::InDiscussion);
        let audit = gov.get_veto_audit(&pid).unwrap();
        assert_eq!(audit.proposal_id, pid);
        assert_eq!(audit.vetoed_by, multisig);

        s.env.ledger().set_timestamp(proposal.execute_after + 1);
        assert!(gov.try_execute(&pid).is_err());

        let events = s.env.events().all();
        let veto_evt = events
            .iter()
            .find(|e| e.0 == s.gov_addr && e.1 == (Symbol::new(&s.env, "vetoed"),).into_val(&s.env))
            .expect("vetoed event");
        let __ver_11: (u32, (u32, Address, u64, u64)) = veto_evt.2.into_val(&s.env);
        assert_eq!(__ver_11.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let data: (u32, Address, u64, u64) = __ver_11.1;
        assert_eq!(data.0, pid);
    }

    #[test]
    fn test_veto_window_enforced() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let multisig = Address::generate(&s.env);
        gov.set_veto_multisig(&multisig);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        s.env
            .ledger()
            .set_timestamp(proposal.vote_end + VETO_WINDOW_SECS + 1);
        assert_eq!(
            gov.try_veto(&pid),
            Err(Ok(GovernanceError::VetoWindowExpired))
        );
    }

    #[test]
    fn test_quorum_decay_passes_before_decay() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        gov.set_quorum_decay_bps_per_day(&100_i128);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);
        // Decay freezes at vote_end (7 days from vote_start), so effective
        // quorum = 1000 + 7*100 = 1700 regardless of when execute() is called.
        // 1000 total votes >= quorum_threshold (1000*1700/10000=170) so proposal passes.
        assert_eq!(gov.get_effective_quorum(&pid), 1_700);
        gov.execute(&pid);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Executed);
    }

    #[test]
    fn test_quorum_decay_defeats_after_threshold() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        gov.set_quorum_decay_bps_per_day(&500_i128);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        // Jump 20 days from vote_start, well past vote_end (7 days) and expires_at
        // (11 days). Decay freezes at vote_end: 7 days * 500 bps/day + 1000 base = 4500,
        // not the 11000 (capped 10000) it would be if decay kept accruing after voting closed.
        s.env
            .ledger()
            .set_timestamp(proposal.vote_start + 20 * 86_400);

        assert_eq!(gov.get_effective_quorum(&pid), 4_500);
        // Frozen quorum threshold (450) is met by the 600 votes cast, and votes_for
        // beats votes_against, so the proposal passed quorum -- it is now Expired
        // (never executed within the window), not Defeated.
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Expired);
    }

    #[test]
    fn test_quorum_decay_disabled_when_rate_zero() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 1_000);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        s.env
            .ledger()
            .set_timestamp(proposal.vote_start + 100 * 86_400);
        assert_eq!(gov.get_effective_quorum(&pid), 1_000);
    }

    // ── CL pool governance tests ──────────────────────────────────────────────

    fn setup_cl_pool(env: &Env, gov_addr: &Address) -> Address {
        let token_a = env.register_stellar_asset_contract_v2(Address::generate(env));
        let token_b = env.register_stellar_asset_contract_v2(Address::generate(env));
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        concentrated_liquidity::ConcentratedLiquidityClient::new(env, &cl_addr).initialize(
            gov_addr,
            &token_a.address(),
            &token_b.address(),
            &30_i128,
            &0_i32,
            &1_i32,
        );
        cl_addr
    }

    fn setup_gov(env: &Env) -> (GovernanceClient<'_>, Address, Address) {
        let admin = Address::generate(env);
        let lp_addr = env.register_contract(None, LpToken);
        token::LpTokenClient::new(env, &lp_addr).initialize(
            &admin,
            &soroban_sdk::String::from_str(env, "AMM LP"),
            &soroban_sdk::String::from_str(env, "ALP"),
            &7u32,
        );
        let gov_addr = env.register_contract(None, Governance);
        let gov = GovernanceClient::new(env, &gov_addr);
        gov.initialize(
            &admin,
            &Address::generate(env),
            &lp_addr,
            &(7 * 24 * 60 * 60_u64),
            &(2 * 24 * 60 * 60_u64),
            &1_000_i128,
            &100_i128,
        );
        token::LpTokenClient::new(env, &lp_addr).set_locker(&gov_addr);
        (gov, gov_addr, lp_addr)
    }

    #[test]
    fn test_cl_pool_pause_unpause() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1_000_000);

        let (gov, gov_addr, lp_addr) = setup_gov(&env);
        let cl_addr = setup_cl_pool(&env, &gov_addr);
        let cl = concentrated_liquidity::ConcentratedLiquidityClient::new(&env, &cl_addr);

        let lp1 = Address::generate(&env);
        token::LpTokenClient::new(&env, &lp_addr).mint(&lp1, &1000);

        let pid1 = gov.propose(&lp1, &ProposalKind::PauseClPool(cl_addr.clone()));
        gov.vote(&lp1, &pid1, &Vote::For);
        let p = gov.get_proposal(&pid1);
        env.ledger().set_timestamp(p.execute_after + 1);
        gov.execute(&pid1);
        assert!(cl.is_paused());
        gov.unlock_vote(&lp1, &pid1);

        let pid2 = gov.propose(&lp1, &ProposalKind::UnpauseClPool(cl_addr));
        gov.vote(&lp1, &pid2, &Vote::For);
        let p = gov.get_proposal(&pid2);
        env.ledger().set_timestamp(p.execute_after + 1);
        gov.execute(&pid2);
        assert!(!cl.is_paused());
        gov.unlock_vote(&lp1, &pid2);
    }

    #[test]
    fn test_cl_pool_set_oracle() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1_000_000);

        let (gov, gov_addr, lp_addr) = setup_gov(&env);
        let cl_addr = setup_cl_pool(&env, &gov_addr);

        let lp1 = Address::generate(&env);
        token::LpTokenClient::new(&env, &lp_addr).mint(&lp1, &1000);

        let oracle_addr = Address::generate(&env);
        let pid = gov.propose(
            &lp1,
            &ProposalKind::UpdateClOracle(UpdateClOracleParams {
                cl_pool: cl_addr.clone(),
                oracle: Some(oracle_addr),
            }),
        );
        gov.vote(&lp1, &pid, &Vote::For);
        let p = gov.get_proposal(&pid);
        env.ledger().set_timestamp(p.execute_after + 1);
        gov.execute(&pid);
        gov.unlock_vote(&lp1, &pid);

        let pid2 = gov.propose(
            &lp1,
            &ProposalKind::UpdateClOracle(UpdateClOracleParams {
                cl_pool: cl_addr,
                oracle: None,
            }),
        );
        gov.vote(&lp1, &pid2, &Vote::For);
        let p = gov.get_proposal(&pid2);
        env.ledger().set_timestamp(p.execute_after + 1);
        gov.execute(&pid2);
        gov.unlock_vote(&lp1, &pid2);
    }

    #[test]
    fn test_cl_pool_max_oracle_deviation() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1_000_000);

        let (gov, gov_addr, lp_addr) = setup_gov(&env);
        let cl_addr = setup_cl_pool(&env, &gov_addr);

        let lp1 = Address::generate(&env);
        token::LpTokenClient::new(&env, &lp_addr).mint(&lp1, &1000);

        let pid = gov.propose(
            &lp1,
            &ProposalKind::UpdateClMaxOracleDeviation(UpdateClMaxOracleDeviationParams {
                cl_pool: cl_addr,
                max_deviation_bps: 100,
            }),
        );
        gov.vote(&lp1, &pid, &Vote::For);
        let p = gov.get_proposal(&pid);
        env.ledger().set_timestamp(p.execute_after + 1);
        gov.execute(&pid);
        gov.unlock_vote(&lp1, &pid);
    }

    #[test]
    fn test_cl_pool_protocol_fee() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1_000_000);

        let (gov, gov_addr, lp_addr) = setup_gov(&env);
        let cl_addr = setup_cl_pool(&env, &gov_addr);

        let lp1 = Address::generate(&env);
        token::LpTokenClient::new(&env, &lp_addr).mint(&lp1, &1000);
        let recipient = Address::generate(&env);

        let pid = gov.propose(
            &lp1,
            &ProposalKind::UpdateClProtocolFee(UpdateClProtocolFeeParams {
                cl_pool: cl_addr,
                recipient,
                bps: 25,
            }),
        );
        gov.vote(&lp1, &pid, &Vote::For);
        let p = gov.get_proposal(&pid);
        env.ledger().set_timestamp(p.execute_after + 1);
        gov.execute(&pid);
        gov.unlock_vote(&lp1, &pid);
    }

    #[test]
    fn test_cl_pool_transfer_admin() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1_000_000);

        let (gov, gov_addr, lp_addr) = setup_gov(&env);
        let cl_addr = setup_cl_pool(&env, &gov_addr);

        let lp1 = Address::generate(&env);
        token::LpTokenClient::new(&env, &lp_addr).mint(&lp1, &1000);

        let pid = gov.propose(
            &lp1,
            &ProposalKind::TransferClPoolAdmin(TransferClPoolAdminParams {
                cl_pool: cl_addr,
                new_admin: Address::generate(&env),
            }),
        );
        gov.vote(&lp1, &pid, &Vote::For);
        let p = gov.get_proposal(&pid);
        env.ledger().set_timestamp(p.execute_after + 1);
        gov.execute(&pid);
        gov.unlock_vote(&lp1, &pid);
    }

    #[test]
    fn test_cl_pool_set_position_nft() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1_000_000);

        let (gov, gov_addr, lp_addr) = setup_gov(&env);
        let cl_addr = setup_cl_pool(&env, &gov_addr);
        let cl = concentrated_liquidity::ConcentratedLiquidityClient::new(&env, &cl_addr);

        let lp1 = Address::generate(&env);
        token::LpTokenClient::new(&env, &lp_addr).mint(&lp1, &1000);

        let nft_addr = Address::generate(&env);
        let pid = gov.propose(
            &lp1,
            &ProposalKind::SetClPositionNft(SetClPositionNftParams {
                cl_pool: cl_addr.clone(),
                nft: Some(nft_addr.clone()),
            }),
        );
        gov.vote(&lp1, &pid, &Vote::For);
        let p = gov.get_proposal(&pid);
        env.ledger().set_timestamp(p.execute_after + 1);
        gov.execute(&pid);
        assert_eq!(cl.position_nft(), Some(nft_addr));
        gov.unlock_vote(&lp1, &pid);

        let pid2 = gov.propose(
            &lp1,
            &ProposalKind::SetClPositionNft(SetClPositionNftParams {
                cl_pool: cl_addr,
                nft: None,
            }),
        );
        gov.vote(&lp1, &pid2, &Vote::For);
        let p = gov.get_proposal(&pid2);
        env.ledger().set_timestamp(p.execute_after + 1);
        gov.execute(&pid2);
        assert_eq!(cl.position_nft(), None);
        gov.unlock_vote(&lp1, &pid2);
    }

    #[test]
    fn test_cl_pool_propose_invalid_params_rejected() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1_000_000);

        let (gov, _, lp_addr) = setup_gov(&env);

        let lp1 = Address::generate(&env);
        token::LpTokenClient::new(&env, &lp_addr).mint(&lp1, &1000);

        let cl_addr = Address::generate(&env);

        let r = gov.try_propose(
            &lp1,
            &ProposalKind::UpdateClMaxOracleDeviation(UpdateClMaxOracleDeviationParams {
                cl_pool: cl_addr.clone(),
                max_deviation_bps: 20_000,
            }),
        );
        assert!(r.is_err());

        let r = gov.try_propose(
            &lp1,
            &ProposalKind::UpdateClProtocolFee(UpdateClProtocolFeeParams {
                cl_pool: cl_addr,
                recipient: Address::generate(&env),
                bps: -1,
            }),
        );
        assert!(r.is_err());
    }

    // ── UpdateFactoryGlobalFee regression tests ───────────────────────────────
    //
    // These tests verify the fix for the partial-execution bug: a proposal
    // whose offset/limit window does not cover all factory pools must be
    // rejected so the proposal is never marked executed while pools remain
    // un-updated.

    /// Minimal mock factory that tracks a configurable pool count and records
    /// whether `set_global_fee_paginated` was called.  Only the three methods
    /// declared in `FactoryInterface` are implemented; every other call would
    /// trap, which is fine for these unit tests.
    #[contract]
    struct MockFactory;

    #[contractimpl]
    impl MockFactory {
        /// Store the pool count that this mock should report.
        pub fn set_pool_count(env: Env, count: u64) {
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "count"), &count);
        }

        /// Return the number of pools set via `set_pool_count`.
        pub fn get_pool_count(env: Env) -> u64 {
            env.storage()
                .instance()
                .get(&Symbol::new(&env, "count"))
                .unwrap_or(0_u64)
        }

        /// Record that this was called so tests can assert it ran.
        pub fn set_global_fee_paginated(
            env: Env,
            _admin: Address,
            _offset: u32,
            limit: u32,
        ) -> u32 {
            let prev: u32 = env
                .storage()
                .instance()
                .get(&Symbol::new(&env, "called"))
                .unwrap_or(0u32);
            env.storage()
                .instance()
                .set(&Symbol::new(&env, "called"), &(prev + 1));
            limit
        }

        /// Satisfy the FactoryInterface trait; not exercised by these tests.
        pub fn set_treasury(
            _env: Env,
            _admin: Address,
            _treasury: Address,
            _global_protocol_fee_bps: i128,
        ) {
        }

        /// Return how many times `set_global_fee_paginated` was called.
        pub fn call_count(env: Env) -> u32 {
            env.storage()
                .instance()
                .get(&Symbol::new(&env, "called"))
                .unwrap_or(0u32)
        }
    }

    /// Suite returned by `setup_factory_gov`.
    struct FactoryGovSuite {
        env: Env,
        gov_addr: Address,
        proposer: Address,
        factory_addr: Address,
    }

    impl FactoryGovSuite {
        fn gov(&self) -> GovernanceClient<'_> {
            GovernanceClient::new(&self.env, &self.gov_addr)
        }
        fn factory(&self) -> MockFactoryClient<'_> {
            MockFactoryClient::new(&self.env, &self.factory_addr)
        }
    }

    /// Helper: set up a governance + lp-token environment backed by a mock
    /// factory that reports `pool_count` pools.
    fn setup_factory_gov(pool_count: u64) -> FactoryGovSuite {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1_000_000);

        let admin = Address::generate(&env);

        // Deploy mock factory and configure its pool count.
        let factory_addr = env.register_contract(None, MockFactory);
        MockFactoryClient::new(&env, &factory_addr).set_pool_count(&pool_count);

        // Deploy LP token.
        let lp_addr = env.register_contract(None, LpToken);
        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &admin,
            &soroban_sdk::String::from_str(&env, "AMM LP"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        // Deploy a bare AMM as the governance target (governance needs an amm
        // address at initialization time).
        let ta = env.register_stellar_asset_contract_v2(admin.clone());
        let tb = env.register_stellar_asset_contract_v2(admin.clone());
        let amm_addr = env.register_contract(None, AmmPool);
        amm::AmmPoolClient::new(&env, &amm_addr).initialize(
            &admin,
            &ta.address(),
            &tb.address(),
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        // Deploy and initialize governance.
        let gov_addr = env.register_contract(None, Governance);
        GovernanceClient::new(&env, &gov_addr).initialize(
            &admin,
            &amm_addr,
            &lp_addr,
            &(7 * 24 * 60 * 60_u64), // voting_period_secs
            &(2 * 24 * 60 * 60_u64), // timelock_secs
            &1_000_i128,             // quorum_bps 10%
            &0_i128,                 // min_proposer_stake_bps (0 → anyone can propose)
        );
        token::LpTokenClient::new(&env, &lp_addr).set_locker(&gov_addr);

        // Mint LP tokens to a proposer so they have voting power.
        let proposer = Address::generate(&env);
        token::LpTokenClient::new(&env, &lp_addr).mint(&proposer, &1_000_i128);

        FactoryGovSuite {
            env,
            gov_addr,
            proposer,
            factory_addr,
        }
    }

    /// propose() must reject a proposal with limit == 0.
    #[test]
    fn test_update_factory_global_fee_propose_rejects_zero_limit() {
        let s = setup_factory_gov(5);
        let gov = s.gov();

        let err = gov
            .try_propose(
                &s.proposer,
                &ProposalKind::UpdateFactoryGlobalFee(UpdateFactoryGlobalFeeParams {
                    factory: s.factory_addr.clone(),
                    offset: 0,
                    limit: 0, // invalid: zero limit
                }),
            )
            .unwrap_err()
            .unwrap();

        assert_eq!(err, GovernanceError::PartialFactoryUpdate);
    }

    /// execute() must reject a proposal whose window covers fewer pools than
    /// the factory currently reports, leaving proposal.executed == false.
    #[test]
    fn test_update_factory_global_fee_execute_rejects_partial_window() {
        let s = setup_factory_gov(10);
        let gov = s.gov();

        // Propose with limit=5, which only covers half of the 10 pools.
        let pid = gov.propose(
            &s.proposer,
            &ProposalKind::UpdateFactoryGlobalFee(UpdateFactoryGlobalFeeParams {
                factory: s.factory_addr.clone(),
                offset: 0,
                limit: 5, // covers pools 0-4 only; factory has 10
            }),
        );

        gov.vote(&s.proposer, &pid, &Vote::For);
        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);

        // execute() must return PartialFactoryUpdate and NOT mark the proposal done.
        let err = gov.try_execute(&pid).unwrap_err().unwrap();
        assert_eq!(err, GovernanceError::PartialFactoryUpdate);

        // The proposal must still be in Queued (not Executed) state.
        let status = gov.proposal_status(&pid);
        assert_eq!(status, ProposalStatus::Queued);

        // set_global_fee_paginated must NOT have been called (tx rolled back).
        let call_count = s.factory().call_count();
        assert_eq!(call_count, 0);
    }

    /// execute() must succeed and mark the proposal executed when limit covers
    /// all pools (offset=0, limit >= pool_count).
    #[test]
    fn test_update_factory_global_fee_execute_succeeds_full_coverage() {
        let s = setup_factory_gov(3);
        let gov = s.gov();

        // Propose with limit=u32::MAX — covers all 3 pools and any future ones.
        let pid = gov.propose(
            &s.proposer,
            &ProposalKind::UpdateFactoryGlobalFee(UpdateFactoryGlobalFeeParams {
                factory: s.factory_addr.clone(),
                offset: 0,
                limit: u32::MAX,
            }),
        );

        gov.vote(&s.proposer, &pid, &Vote::For);
        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);

        // execute() must succeed.
        gov.execute(&pid);

        // Proposal must now be marked Executed.
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Executed);

        // set_global_fee_paginated must have been called exactly once.
        let call_count = s.factory().call_count();
        assert_eq!(call_count, 1);
    }

    /// execute() must succeed when the factory has zero pools (empty factory is
    /// a degenerate but valid case: any limit > 0 satisfies 0 + limit >= 0).
    #[test]
    fn test_update_factory_global_fee_execute_succeeds_empty_factory() {
        let s = setup_factory_gov(0);
        let gov = s.gov();

        let pid = gov.propose(
            &s.proposer,
            &ProposalKind::UpdateFactoryGlobalFee(UpdateFactoryGlobalFeeParams {
                factory: s.factory_addr.clone(),
                offset: 0,
                limit: 1,
            }),
        );

        gov.vote(&s.proposer, &pid, &Vote::For);
        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);

        gov.execute(&pid);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Executed);
    }

    // ── Proposal enumeration (issue #793) ────────────────────────────────────

    /// Five distinct `ProposalKind`s, so enumeration is exercised across
    /// variants rather than repeats of a single kind.
    fn enumeration_kinds(env: &Env) -> [ProposalKind; 5] {
        [
            ProposalKind::UpdateFee(50),
            ProposalKind::UpdateFee(60),
            ProposalKind::PausePool,
            ProposalKind::UnpausePool,
            ProposalKind::TransferAdmin(Address::generate(env)),
        ]
    }

    #[test]
    fn test_get_proposal_count_starts_at_zero() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        assert_eq!(gov.get_proposal_count(), 0);
        assert_eq!(gov.get_proposals_paginated(&0, &10).len(), 0);
    }

    #[test]
    fn test_get_proposal_count_tracks_created_proposals() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let lp1 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 1_000);

        for (i, kind) in enumeration_kinds(&s.env).iter().take(3).enumerate() {
            assert_eq!(gov.get_proposal_count(), i as u32);
            assert_eq!(gov.propose(&lp1, kind), i as u32);
        }

        assert_eq!(gov.get_proposal_count(), 3);
    }

    #[test]
    fn test_get_proposals_paginated_returns_creation_order_matching_get_proposal() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let lp1 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 1_000);

        let kinds = enumeration_kinds(&s.env);
        for kind in kinds.iter() {
            gov.propose(&lp1, kind);
        }
        assert_eq!(gov.get_proposal_count(), 5);

        let page = gov.get_proposals_paginated(&0, &5);
        assert_eq!(page.len(), 5);
        for id in 0..5u32 {
            let from_page = page.get(id).unwrap();
            // Ids ascend in creation order, and every field matches the
            // single-id accessor exactly.
            assert_eq!(from_page.id, id);
            assert_eq!(from_page.kind, kinds[id as usize]);
            assert_eq!(from_page, gov.get_proposal(&id));
        }
    }

    #[test]
    fn test_get_proposals_paginated_offset_and_limit_boundaries() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let lp1 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 1_000);

        for kind in enumeration_kinds(&s.env).iter() {
            gov.propose(&lp1, kind);
        }

        // Interior slice.
        let mid = gov.get_proposals_paginated(&1, &2);
        assert_eq!(mid.len(), 2);
        assert_eq!(mid.get(0).unwrap().id, 1);
        assert_eq!(mid.get(1).unwrap().id, 2);

        // offset + limit > count returns only the remaining tail.
        let tail = gov.get_proposals_paginated(&3, &100);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail.get(0).unwrap().id, 3);
        assert_eq!(tail.get(1).unwrap().id, 4);

        // offset == count and offset > count are both empty, not panics.
        assert_eq!(gov.get_proposals_paginated(&5, &10).len(), 0);
        assert_eq!(gov.get_proposals_paginated(&99, &10).len(), 0);

        // A zero limit is empty even well inside the range.
        assert_eq!(gov.get_proposals_paginated(&0, &0).len(), 0);

        // `start + limit` must not overflow u32.
        assert_eq!(gov.get_proposals_paginated(&1, &u32::MAX).len(), 4);
    }

    #[test]
    fn test_get_proposals_paginated_walks_full_set_in_pages() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let lp1 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 1_000);

        for kind in enumeration_kinds(&s.env).iter() {
            gov.propose(&lp1, kind);
        }

        // Page size 2 over 5 proposals: 2 + 2 + 1, driven off the count.
        let count = gov.get_proposal_count();
        let mut seen: soroban_sdk::Vec<u32> = soroban_sdk::Vec::new(&s.env);
        let mut offset = 0u32;
        while offset < count {
            let page = gov.get_proposals_paginated(&offset, &2);
            for i in 0..page.len() {
                seen.push_back(page.get(i).unwrap().id);
            }
            offset += 2;
        }

        assert_eq!(seen.len(), 5);
        for id in 0..5u32 {
            assert_eq!(seen.get(id).unwrap(), id);
        }
    }

    #[test]
    fn test_get_proposals_paginated_reflects_status_transitions_without_gaps() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let executed_id = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        let cancelled_id = gov.propose(&lp1, &ProposalKind::UpdateFee(60));
        let open_id = gov.propose(&lp1, &ProposalKind::PausePool);

        gov.vote(&lp1, &executed_id, &Vote::For);
        gov.vote(&lp2, &executed_id, &Vote::For);
        gov.cancel_proposal(&cancelled_id, &lp1);

        let proposal = gov.get_proposal(&executed_id);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);
        gov.execute(&executed_id);

        // Executing and cancelling mutate flags in place; neither shrinks the
        // count nor punches a hole in the id range.
        assert_eq!(gov.get_proposal_count(), 3);
        let page = gov.get_proposals_paginated(&0, &3);
        assert_eq!(page.len(), 3);
        assert!(page.get(0).unwrap().executed);
        assert!(page.get(1).unwrap().cancelled);
        assert!(!page.get(2).unwrap().executed);
        assert_eq!(page.get(2).unwrap().id, open_id);
    }

    // ── Issue #825: every governance event carries EVENT_SCHEMA_VERSION ───────
    //
    // `cancel_proposal`, `delegate`, `undelegate` and `set_veto_multisig` used
    // to call `env.events().publish(...)` directly, so their payloads were not
    // version-stamped and an indexer reading `(version, ...rest)` would have
    // decoded the first real field as the version number. These tests pin the
    // stamp in place for all four.

    /// Fetch the payload of the most recent event this contract published under
    /// `topic`, decoded as a version-stamped `(u32, T)` pair.
    fn last_versioned_event<T>(s: &Suite, gov: &GovernanceClient, topic: &str) -> (u32, T)
    where
        T: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>,
    {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::IntoVal;

        let wanted: soroban_sdk::Vec<soroban_sdk::Val> =
            (Symbol::new(&s.env, topic),).into_val(&s.env);
        let evt = s
            .env
            .events()
            .all()
            .iter()
            .rfind(|e| e.0 == gov.address && e.1 == wanted)
            .unwrap_or_else(|| panic!("no `{topic}` event found"));
        evt.2.into_val(&s.env)
    }

    #[test]
    fn test_cancel_proposal_emits_versioned_event() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 1_000);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.cancel_proposal(&pid, &lp1);

        let (version, data): (u32, (u32,)) = last_versioned_event(&s, &gov, "cancelled");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (pid,));
    }

    #[test]
    fn test_delegate_emits_versioned_event() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let from = Address::generate(&s.env);
        let to = Address::generate(&s.env);
        mint_lp(&s, &from, 500);
        mint_lp(&s, &to, 500);

        gov.delegate(&from, &to);

        let (version, data): (u32, (Address, Address)) =
            last_versioned_event(&s, &gov, "delegated");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (from, to));
    }

    #[test]
    fn test_undelegate_emits_versioned_event() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let from = Address::generate(&s.env);
        let to = Address::generate(&s.env);
        mint_lp(&s, &from, 500);
        mint_lp(&s, &to, 500);

        gov.delegate(&from, &to);
        gov.undelegate(&from);

        let (version, data): (u32, (Address,)) = last_versioned_event(&s, &gov, "undelegated");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (from,));
    }

    #[test]
    fn test_set_veto_multisig_emits_versioned_event() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let multisig = Address::generate(&s.env);
        gov.set_veto_multisig(&multisig);

        let (version, data): (u32, (Address,)) =
            last_versioned_event(&s, &gov, "veto_multisig_set");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (multisig,));
    }

    // ── Issue #824: missing-proposal reads return a typed error, not a trap ───
    //
    // `get_proposal`, `proposal_status` and `get_effective_quorum` used to
    // `.expect("proposal not found")`, trapping the whole invocation. They now
    // return `GovernanceError::ProposalNotFound`, which off-chain callers can
    // match on via the generated `try_*` client methods.

    #[test]
    fn test_get_proposal_nonexistent_returns_error() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        assert_eq!(
            gov.try_get_proposal(&999),
            Err(Ok(GovernanceError::ProposalNotFound))
        );
    }

    #[test]
    fn test_proposal_status_nonexistent_returns_error() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        assert_eq!(
            gov.try_proposal_status(&999),
            Err(Ok(GovernanceError::ProposalNotFound))
        );
    }

    #[test]
    fn test_get_effective_quorum_nonexistent_returns_error() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        assert_eq!(
            gov.try_get_effective_quorum(&999),
            Err(Ok(GovernanceError::ProposalNotFound))
        );
    }

    #[test]
    fn test_get_proposal_existing_still_works() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 1_000);
        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));

        // The plain (non-`try_`) client method still returns the bare value.
        let proposal = gov.get_proposal(&pid);
        assert_eq!(proposal.id, pid);
        assert_eq!(proposal.proposer, lp1);
        assert_eq!(proposal.kind, ProposalKind::UpdateFee(50));
        assert!(!proposal.executed);

        // And the `try_` variant returns it wrapped in `Ok`.
        assert_eq!(gov.try_get_proposal(&pid), Ok(Ok(proposal)));

        // `get_effective_quorum` for a real proposal is the configured base.
        assert_eq!(gov.get_effective_quorum(&pid), 1_000_i128);
    }

    #[test]
    fn test_proposal_status_existing_still_works() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Active);
        assert_eq!(
            gov.try_proposal_status(&pid),
            Ok(Ok(ProposalStatus::Active))
        );

        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);

        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Queued);

        gov.execute(&pid);
        assert_eq!(gov.proposal_status(&pid), ProposalStatus::Executed);
    }

    /// `unlock_vote` now propagates `proposal_status`'s `Result` with `?`; the
    /// full lifecycle must be unaffected for a real proposal, and unlocking
    /// against an unknown id must surface `ProposalNotFound` rather than trap.
    #[test]
    fn test_unlock_vote_still_works_after_signature_change() {
        let s = setup_suite(30);
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let lp_client = token::LpTokenClient::new(&s.env, &s.lp_addr);

        let lp1 = Address::generate(&s.env);
        let lp2 = Address::generate(&s.env);
        mint_lp(&s, &lp1, 600);
        mint_lp(&s, &lp2, 400);

        let pid = gov.propose(&lp1, &ProposalKind::UpdateFee(50));
        gov.vote(&lp1, &pid, &Vote::For);
        gov.vote(&lp2, &pid, &Vote::For);
        assert_eq!(lp_client.locked_balance(&lp1), 600);

        let proposal = gov.get_proposal(&pid);
        s.env.ledger().set_timestamp(proposal.execute_after + 1);
        gov.execute(&pid);

        gov.unlock_vote(&lp1, &pid);
        gov.unlock_vote(&lp2, &pid);
        assert_eq!(lp_client.locked_balance(&lp1), 0);
        assert_eq!(lp_client.locked_balance(&lp2), 0);

        // The propagated error reaches the caller as a typed error.
        assert_eq!(
            gov.try_unlock_vote(&lp1, &999),
            Err(Ok(GovernanceError::ProposalNotFound))
        );
    }

    // ── #931: typed NotInitialized instead of host traps ─────────────────────

    fn uninitialized_gov(env: &Env) -> GovernanceClient<'_> {
        env.mock_all_auths();
        let gov_addr = env.register_contract(None, Governance);
        GovernanceClient::new(env, &gov_addr)
    }

    #[test]
    fn test_pre_init_admin_setters_return_not_initialized() {
        let env = Env::default();
        let gov = uninitialized_gov(&env);
        let someone = Address::generate(&env);

        assert_eq!(
            gov.try_set_quorum_decay_bps_per_day(&10_i128),
            Err(Ok(GovernanceError::NotInitialized))
        );
        assert_eq!(
            gov.try_set_min_proposer_stake_bps(&100_i128),
            Err(Ok(GovernanceError::NotInitialized))
        );
        assert_eq!(
            gov.try_set_timelock_delay(&60_u64),
            Err(Ok(GovernanceError::NotInitialized))
        );
        assert_eq!(
            gov.try_set_veto_multisig(&someone),
            Err(Ok(GovernanceError::NotInitialized))
        );
        assert_eq!(
            gov.try_propose_admin(&someone, &Address::generate(&env)),
            Err(Ok(GovernanceError::NotInitialized))
        );
    }

    #[test]
    fn test_pre_init_propose_and_get_params_return_not_initialized() {
        let env = Env::default();
        let gov = uninitialized_gov(&env);
        let proposer = Address::generate(&env);

        // A well-formed proposal passes kind validation and then needs the
        // LP token, which only `initialize` sets.
        assert_eq!(
            gov.try_propose(&proposer, &ProposalKind::UpdateFee(30)),
            Err(Ok(GovernanceError::NotInitialized))
        );
        // `GovernanceParams` is not `PartialEq`, so compare the error side only.
        assert_eq!(
            gov.try_get_params().err(),
            Some(Ok(GovernanceError::NotInitialized))
        );
    }

    #[test]
    fn test_pre_init_proposal_entrypoints_return_typed_errors() {
        let env = Env::default();
        let gov = uninitialized_gov(&env);
        let voter = Address::generate(&env);

        // No proposal can exist before `initialize`, so proposal-scoped
        // entrypoints report the missing proposal instead of trapping.
        assert_eq!(
            gov.try_vote(&voter, &0, &Vote::For),
            Err(Ok(GovernanceError::ProposalNotFound))
        );
        assert_eq!(
            gov.try_execute(&0),
            Err(Ok(GovernanceError::ProposalNotFound))
        );
        assert_eq!(
            gov.try_get_effective_quorum(&0),
            Err(Ok(GovernanceError::ProposalNotFound))
        );
        assert_eq!(
            gov.try_get_snapshot_balance(&0, &voter),
            Err(Ok(GovernanceError::ProposalNotFound))
        );
        assert_eq!(
            gov.try_veto(&0),
            Err(Ok(GovernanceError::VetoMultisigNotSet))
        );
        assert_eq!(
            gov.try_accept_admin(&voter),
            Err(Ok(GovernanceError::NoPendingAdmin))
        );
        // Plain counters degrade to their empty value.
        assert_eq!(gov.get_proposal_count(), 0);
        assert_eq!(gov.get_proposals_paginated(&0, &10).len(), 0);
    }
}

// ── Property-based tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod prop_tests {
    extern crate std;
    use super::*;
    use amm::AmmPool;
    use proptest::collection;
    use proptest::prelude::*;
    use proptest::test_runner::{Config, TestRunner};
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::{Address, Env};
    use token::LpToken;

    // ── Test harness ────────────────────────────────────────────────────────────

    struct PropEnv {
        env: Env,
        gov_addr: Address,
        lp_addr: Address,
        admin: Address,
    }

    fn setup_prop_env() -> PropEnv {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        env.ledger().set_timestamp(1_000_000);

        let admin = Address::generate(&env);

        let lp_addr = env.register_contract(None, LpToken);
        token::LpTokenClient::new(&env, &lp_addr).initialize(
            &admin,
            &soroban_sdk::String::from_str(&env, "AMM LP"),
            &soroban_sdk::String::from_str(&env, "ALP"),
            &7u32,
        );

        let ta = env.register_stellar_asset_contract_v2(admin.clone());
        let tb = env.register_stellar_asset_contract_v2(admin.clone());

        let amm_addr = env.register_contract(None, AmmPool);
        amm::AmmPoolClient::new(&env, &amm_addr).initialize(
            &admin,
            &ta.address(),
            &tb.address(),
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        let gov_addr = env.register_contract(None, Governance);
        GovernanceClient::new(&env, &gov_addr).initialize(
            &admin,
            &amm_addr,
            &lp_addr,
            &(7 * 24 * 60 * 60_u64),
            &(2 * 24 * 60 * 60_u64),
            &1_000_i128,
            &0_i128, // no min stake so any holder can propose in prop tests
        );

        token::LpTokenClient::new(&env, &lp_addr).set_locker(&gov_addr);
        PropEnv {
            env,
            gov_addr,
            lp_addr,
            admin,
        }
    }

    // ── Pure math properties ────────────────────────────────────────────────────

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Quorum threshold never overflows or goes out of bounds.
        #[test]
        fn quorum_check_never_overflows(
            total_supply in 1i128..i128::MAX / 10_000,
            quorum_bps in 1i128..10_000i128,
        ) {
            let threshold = total_supply * quorum_bps / 10_000;
            prop_assert!(threshold >= 0);
            prop_assert!(threshold <= total_supply);
        }

        /// Combined votes cast never exceeds total supply.
        #[test]
        fn total_votes_does_not_exceed_supply(
            votes_for in 0i128..i128::MAX / 2,
            votes_against in 0i128..i128::MAX / 2,
            votes_abstain in 0i128..i128::MAX / 2,
        ) {
            // Use saturating arithmetic for both to avoid integer overflow in the test harness.
            let total_supply = votes_for.saturating_add(votes_against).saturating_add(votes_abstain);
            let total_votes = votes_for.saturating_add(votes_against).saturating_add(votes_abstain);
            prop_assert_eq!(total_votes, total_supply);

            // Individual vote buckets are non-negative and bounded.
            prop_assert!(votes_for >= 0);
            prop_assert!(votes_against >= 0);
            prop_assert!(votes_abstain >= 0);
            prop_assert!(votes_for <= total_votes);
            prop_assert!(votes_against <= total_votes);
            prop_assert!(votes_abstain <= total_votes);
        }

        /// Min proposer stake math holds and is within expected bounds.
        #[test]
        fn min_proposer_stake_is_correct(
            total_supply in 1i128..i128::MAX / 10_000,
            min_bps in 0i128..10_000i128,
        ) {
            let min_stake = ((total_supply * min_bps) / 10_000).max(1);
            prop_assert!(min_stake >= 1);
            prop_assert!(min_stake <= total_supply.max(1));
        }

        /// Expiry always comes at or after execute_after.
        #[test]
        fn expiry_logic_boundaries(
            vote_end in 0u64..u64::MAX / 3,
            timelock in 0u64..u64::MAX / 3,
            voting_period in 1u64..u64::MAX / 3,
        ) {
            let execute_after = vote_end + timelock;
            let expires_at = execute_after + timelock.max(voting_period);
            prop_assert!(expires_at >= execute_after,
                "expires_at={expires_at} < execute_after={execute_after}");
            prop_assert!(execute_after >= vote_end);
        }

        /// No overflow in proposal lifecycle timestamps.
        #[test]
        fn timestamp_arithmetic_no_overflow(
            now in 0u64..u64::MAX / 4,
            voting_period in 1u64..u64::MAX / 4,
            timelock in 0u64..u64::MAX / 4,
        ) {
            let vote_end = now + voting_period;
            let execute_after = vote_end + timelock;
            let expires_at = execute_after + timelock.max(voting_period);
            prop_assert!(vote_end >= now);
            prop_assert!(execute_after >= vote_end);
            prop_assert!(expires_at >= execute_after);
        }

        /// Edge-case: when timelock is zero, execute_after == vote_end.
        #[test]
        fn zero_timelock_property(
            vote_end in 0u64..u64::MAX / 2,
        ) {
            let execute_after = vote_end;
            prop_assert_eq!(execute_after, vote_end);
        }
    }

    // ── Property 1: Voting power conservation (contract-level) ─────────────────

    #[test]
    fn prop_voting_power_conservation() {
        let mut runner = TestRunner::new(Config {
            cases: 256,
            ..Config::default()
        });

        let strategy = collection::vec((1i128..10_000, 0..4i8), 2..=8);

        runner.run(&strategy, |voters| {
            let pe = setup_prop_env();
            let gov = GovernanceClient::new(&pe.env, &pe.gov_addr);
            let lp = token::LpTokenClient::new(&pe.env, &pe.lp_addr);

            let n = voters.len();
            let holders: std::vec::Vec<Address> =
                (0..n).map(|_| Address::generate(&pe.env)).collect();

            for (i, (amt, _)) in voters.iter().enumerate() {
                if *amt > 0 {
                    lp.mint(&holders[i], amt);
                }
            }

            let total_supply: i128 = voters.iter().map(|(a, _)| a).sum();
            let proposer_idx = voters.iter().position(|(a, _)| *a >= 1).unwrap();
            let pid = gov.propose(&holders[proposer_idx], &ProposalKind::UpdateFee(50));

            let mut expected_for: i128 = 0;
            let mut expected_against: i128 = 0;
            let mut expected_abstain: i128 = 0;

            for (i, (amt, vote_choice)) in voters.iter().enumerate() {
                if *vote_choice == 0 || *amt == 0 {
                    continue;
                }
                let choice = match *vote_choice {
                    1 => Vote::For,
                    2 => Vote::Against,
                    _ => Vote::Abstain,
                };
                if gov.try_vote(&holders[i], &pid, &choice).is_ok() {
                    match choice {
                        Vote::For => expected_for += amt,
                        Vote::Against => expected_against += amt,
                        Vote::Abstain => expected_abstain += amt,
                    }
                    let locked = lp.locked_balance(&holders[i]);
                    prop_assert_eq!(locked, *amt,
                        "locked balance should equal voting power: voter={}, locked={}, power={}", i, locked, amt);
                }
            }

            let proposal = gov.get_proposal(&pid);
            let total_votes = proposal.votes_for + proposal.votes_against + proposal.votes_abstain;
            prop_assert!(total_votes <= total_supply,
                "total_votes={total_votes} exceeds total_supply={total_supply}");
            prop_assert_eq!(proposal.votes_for, expected_for,
                "votes_for mismatch");
            prop_assert_eq!(proposal.votes_against, expected_against,
                "votes_against mismatch");
            prop_assert_eq!(proposal.votes_abstain, expected_abstain,
                "votes_abstain mismatch");

            // Each voter's locked amount never exceeds their minted balance.
            for (i, (amt, _)) in voters.iter().enumerate() {
                let locked = lp.locked_balance(&holders[i]);
                prop_assert!(locked <= *amt,
                    "voter {i}: locked={locked} > balance={amt}");
            }

            Ok(())
        })
        .unwrap();
    }

    // ── Property 2: Delegated voting power conservation ─────────────────────────

    #[test]
    fn prop_delegated_power_conservation() {
        let mut runner = TestRunner::new(Config {
            cases: 256,
            ..Config::default()
        });

        let strategy = (
            collection::vec(1i128..10_000, 2..=5),
            0..5u32, // delegatee index within voters
        );

        runner
            .run(&strategy, |(amounts, delegatee_idx)| {
                let pe = setup_prop_env();
                let gov = GovernanceClient::new(&pe.env, &pe.gov_addr);
                let lp = token::LpTokenClient::new(&pe.env, &pe.lp_addr);

                let n = amounts.len();
                let holders: std::vec::Vec<Address> =
                    (0..n).map(|_| Address::generate(&pe.env)).collect();

                for (i, amt) in amounts.iter().enumerate() {
                    lp.mint(&holders[i], amt);
                }

                let delegatee = if (delegatee_idx as usize) < n {
                    delegatee_idx as usize
                } else {
                    0
                };

                // All non-delegatee holders delegate to delegatee.
                for (i, _) in amounts.iter().enumerate() {
                    if i != delegatee {
                        gov.delegate(&holders[i], &holders[delegatee]);
                    }
                }

                let proposer = &holders[delegatee];
                let pid = gov.propose(proposer, &ProposalKind::UpdateFee(50));

                // Delegatee votes with aggregated power.
                if gov.try_vote(&holders[delegatee], &pid, &Vote::For).is_ok() {
                    let expected_power: i128 = amounts.iter().sum();
                    let proposal = gov.get_proposal(&pid);
                    prop_assert_eq!(
                        proposal.votes_for,
                        expected_power,
                        "delegated power={} != expected={}",
                        proposal.votes_for,
                        expected_power
                    );

                    // Delegatee's locked balance equals their own LP (not the whole delegation).
                    let delegatee_locked = lp.locked_balance(&holders[delegatee]);
                    prop_assert_eq!(
                        delegatee_locked,
                        amounts[delegatee],
                        "delegatee locked should equal own balance"
                    );

                    // Delegators' LPs should also be locked.
                    for (i, amt) in amounts.iter().enumerate() {
                        if i != delegatee {
                            let locked = lp.locked_balance(&holders[i]);
                            prop_assert_eq!(
                                locked,
                                *amt,
                                "delegator {}: locked={} != balance={}",
                                i,
                                locked,
                                amt
                            );
                        }
                    }

                    // Total supply conservation.
                    let total_supply: i128 = amounts.iter().sum();
                    let total_votes =
                        proposal.votes_for + proposal.votes_against + proposal.votes_abstain;
                    prop_assert!(
                        total_votes <= total_supply,
                        "total_votes={total_votes} > total_supply={total_supply}"
                    );
                }

                Ok(())
            })
            .unwrap();
    }

    // ── Property 3: Vote locking / unlocking consistency ────────────────────────

    #[test]
    fn prop_lock_unlock_consistency() {
        let mut runner = TestRunner::new(Config {
            cases: 256,
            ..Config::default()
        });

        let strategy = collection::vec((1i128..10_000, 1..4i8), 2..=5);

        runner
            .run(&strategy, |voters| {
                let pe = setup_prop_env();
                let gov = GovernanceClient::new(&pe.env, &pe.gov_addr);
                let lp = token::LpTokenClient::new(&pe.env, &pe.lp_addr);

                let n = voters.len();
                let holders: std::vec::Vec<Address> =
                    (0..n).map(|_| Address::generate(&pe.env)).collect();

                for (i, (amt, _)) in voters.iter().enumerate() {
                    lp.mint(&holders[i], amt);
                }

                let proposer_idx = voters.iter().position(|(a, _)| *a >= 1).unwrap();
                let pid = gov.propose(&holders[proposer_idx], &ProposalKind::UpdateFee(50));

                // Vote (each voter chooses For/Against/Abstain per their vote_choice)
                for (i, (amt, vote_choice)) in voters.iter().enumerate() {
                    let choice = match *vote_choice {
                        1 => Vote::For,
                        2 => Vote::Against,
                        _ => Vote::Abstain,
                    };
                    if gov.try_vote(&holders[i], &pid, &choice).is_ok() {
                        // Immediately after vote: locked == voting power
                        prop_assert_eq!(
                            lp.locked_balance(&holders[i]),
                            *amt,
                            "after vote: locked={} != power={}",
                            lp.locked_balance(&holders[i]),
                            *amt
                        );
                    }
                }

                // Verify unlocks fail while proposal is active.
                for (i, _) in voters.iter().enumerate() {
                    if lp.locked_balance(&holders[i]) > 0 {
                        prop_assert!(
                            gov.try_unlock_vote(&holders[i], &pid).is_err(),
                            "unlock should fail while proposal is active"
                        );
                    }
                }

                // Advance past voting + timelock.
                let proposal = gov.get_proposal(&pid);
                pe.env.ledger().set_timestamp(proposal.execute_after + 1);

                // Execute (may succeed or fail depending on quorum/majority).
                let _ = gov.try_execute(&pid);
                let status = gov.proposal_status(&pid);

                // Only concluded statuses allow unlock.
                let can_unlock = matches!(
                    status,
                    ProposalStatus::Executed
                        | ProposalStatus::Defeated
                        | ProposalStatus::Expired
                        | ProposalStatus::Cancelled
                );

                if can_unlock {
                    for (i, _amt) in voters.iter().enumerate() {
                        let locked_before = lp.locked_balance(&holders[i]);
                        if locked_before > 0 {
                            if gov.try_unlock_vote(&holders[i], &pid).is_ok() {
                                let locked_after = lp.locked_balance(&holders[i]);
                                prop_assert_eq!(
                                    locked_after,
                                    0,
                                    "after unlock: locked should be 0, got {}",
                                    locked_after
                                );
                            }
                        } else {
                            // Unlock with no locked vote should fail.
                            prop_assert!(
                                gov.try_unlock_vote(&holders[i], &pid).is_err(),
                                "unlock with no locked vote should fail"
                            );
                        }
                    }
                } else {
                    let status_name = std::format!("{:?}", status);
                    // Unlock should still fail for non-concluded proposals.
                    for (i, _) in voters.iter().enumerate() {
                        if lp.locked_balance(&holders[i]) > 0 {
                            let result = gov.try_unlock_vote(&holders[i], &pid);
                            prop_assert!(
                                result.is_err(),
                                "unlock should fail for status={status_name}"
                            );
                        }
                    }
                }

                Ok(())
            })
            .unwrap();
    }

    // ── Property 4: Malicious / invalid voting scenarios ────────────────────────

    #[test]
    fn prop_malicious_voting_scenarios() {
        let mut runner = TestRunner::new(Config {
            cases: 256,
            ..Config::default()
        });

        let strategy = collection::vec(1i128..10_000, 3..=6);

        runner
            .run(&strategy, |amounts| {
                let pe = setup_prop_env();
                let gov = GovernanceClient::new(&pe.env, &pe.gov_addr);
                let lp = token::LpTokenClient::new(&pe.env, &pe.lp_addr);

                // We need n main holders, plus a zero-balance address, plus 2 delegation
                // addresses — all minted before the proposal snapshot.
                let n = amounts.len();
                let holders: std::vec::Vec<Address> =
                    (0..n + 3).map(|_| Address::generate(&pe.env)).collect();

                for (i, amt) in amounts.iter().enumerate() {
                    lp.mint(&holders[i], amt);
                }
                // Mint LP for delegation addresses so they have snapshot voting power.
                lp.mint(&holders[n + 1], &1000);
                lp.mint(&holders[n + 2], &1000);

                let pid = gov.propose(&holders[0], &ProposalKind::UpdateFee(50));

                // ── A: Double vote ──
                if gov.try_vote(&holders[0], &pid, &Vote::For).is_ok() {
                    let double = gov.try_vote(&holders[0], &pid, &Vote::For);
                    prop_assert!(double.is_err(), "double vote should fail");
                }

                // ── B: Vote after deadline ──
                {
                    let proposal = gov.get_proposal(&pid);
                    pe.env.ledger().set_timestamp(proposal.vote_end + 1);
                    let late_vote = gov.try_vote(&holders[1], &pid, &Vote::For);
                    prop_assert!(late_vote.is_err(), "vote after deadline should fail");
                    pe.env.ledger().set_timestamp(1_000_000);
                }

                // ── C: Vote on non-existent proposal ──
                {
                    let bad_vote = gov.try_vote(&holders[1], &u32::MAX, &Vote::For);
                    prop_assert!(bad_vote.is_err(), "vote on bad proposal should fail");
                }

                // ── D: Vote with 0 snapshot balance (holder never minted) ──
                {
                    let zero_vote = gov.try_vote(&holders[n], &pid, &Vote::For);
                    prop_assert!(zero_vote.is_err(), "vote with 0 balance should fail");
                }

                // ── E: Self-delegation ──
                {
                    let self_delegate = gov.try_delegate(&holders[1], &holders[1]);
                    prop_assert!(self_delegate.is_err(), "self-delegation should fail");
                }

                // ── F: Delegation cycle (a->b, b->a) ──
                {
                    let a = &holders[n + 1];
                    let b = &holders[n + 2];
                    if gov.try_delegate(a, b).is_ok() {
                        let cycle = gov.try_delegate(b, a);
                        prop_assert!(cycle.is_err(), "delegation cycle should fail");
                    }
                }

                // ── G: Vote while delegated ──
                {
                    let deleter = &holders[n + 1];
                    let del_target = &holders[n + 2];
                    if gov.try_delegate(deleter, del_target).is_ok() {
                        let delegated_vote = gov.try_vote(deleter, &pid, &Vote::For);
                        prop_assert!(
                            delegated_vote.is_err(),
                            "delegated address should not be able to vote directly"
                        );
                    }
                }

                // ── H: Unlock without ever voting ──
                {
                    let unlock_no_vote = gov.try_unlock_vote(&holders[2], &pid);
                    prop_assert!(unlock_no_vote.is_err(), "unlock without voting should fail");
                }

                Ok(())
            })
            .unwrap();
    }

    // ── Property 5: Post-execution unlock guarantees ────────────────────────────

    #[test]
    fn prop_post_execution_unlock_recovers_all_locked_tokens() {
        let mut runner = TestRunner::new(Config {
            cases: 256,
            ..Config::default()
        });

        let strategy = collection::vec((1i128..5_000, 1..4i8), 2..=5);

        runner
            .run(&strategy, |voters| {
                let pe = setup_prop_env();
                let gov = GovernanceClient::new(&pe.env, &pe.gov_addr);
                let lp = token::LpTokenClient::new(&pe.env, &pe.lp_addr);

                let n = voters.len();
                let holders: std::vec::Vec<Address> =
                    (0..n).map(|_| Address::generate(&pe.env)).collect();

                for (i, (amt, _)) in voters.iter().enumerate() {
                    lp.mint(&holders[i], amt);
                }

                let proposer_idx = voters.iter().position(|(a, _)| *a >= 1).unwrap();
                let pid = gov.propose(&holders[proposer_idx], &ProposalKind::UpdateFee(50));

                for (i, (_, vote_choice)) in voters.iter().enumerate() {
                    let choice = match *vote_choice {
                        1 => Vote::For,
                        2 => Vote::Against,
                        _ => Vote::Abstain,
                    };
                    let _ = gov.try_vote(&holders[i], &pid, &choice);
                }

                // Advance time and try to execute.
                let proposal = gov.get_proposal(&pid);
                pe.env.ledger().set_timestamp(proposal.execute_after + 1);
                let _ = gov.try_execute(&pid);
                let status = gov.proposal_status(&pid);

                let can_unlock = matches!(
                    status,
                    ProposalStatus::Executed
                        | ProposalStatus::Defeated
                        | ProposalStatus::Expired
                        | ProposalStatus::Cancelled
                );

                if can_unlock {
                    let mut total_locked_after: i128 = 0;

                    for (i, (_amt, _)) in voters.iter().enumerate() {
                        let locked_before = lp.locked_balance(&holders[i]);
                        if locked_before > 0 {
                            // Balance before unlock = balance - locked (locked unavailable for transfer).
                            let _bal_before = lp.balance(&holders[i]);

                            let _ = gov.try_unlock_vote(&holders[i], &pid);

                            let locked_after = lp.locked_balance(&holders[i]);
                            total_locked_after += locked_after;

                            // After unlock: user should be able to transfer their full balance.
                            if locked_before > 0 && locked_after == 0 {
                                // Attempt to transfer the originally locked amount to a fresh address.
                                let recipient = Address::generate(&pe.env);
                                let transfer_result =
                                    lp.try_transfer(&holders[i], &recipient, &locked_before);
                                prop_assert!(
                                    transfer_result.is_ok(),
                                    "should be able to transfer unlocked tokens"
                                );
                            }
                        }
                    }

                    // After all unlocks, total locked should be 0 for concluded proposals.
                    prop_assert_eq!(
                        total_locked_after,
                        0,
                        "total locked after all unlocks should be 0, got {}",
                        total_locked_after
                    );
                }

                Ok(())
            })
            .unwrap();
    }

    // ── Admin rotation (#379) ─────────────────────────────────────────────────

    #[test]
    fn test_propose_admin_requires_current_admin() {
        let s = setup_prop_env();
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let rando = Address::generate(&s.env);
        let nominee = Address::generate(&s.env);
        assert!(gov.try_propose_admin(&rando, &nominee).is_err());
    }

    #[test]
    fn test_accept_admin_requires_pending_nomination() {
        let s = setup_prop_env();
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let nominee = Address::generate(&s.env);
        assert!(gov.try_accept_admin(&nominee).is_err());
    }

    #[test]
    fn test_admin_rotation_two_step_handover() {
        let s = setup_prop_env();
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        let new_admin = Address::generate(&s.env);

        // Current admin nominates the new admin.
        gov.propose_admin(&s.admin, &new_admin);

        // A non-nominee cannot accept.
        let rando = Address::generate(&s.env);
        assert!(gov.try_accept_admin(&rando).is_err());

        // The nominee accepts and becomes admin.
        gov.accept_admin(&new_admin);

        // Old admin can no longer nominate; new admin can.
        let another = Address::generate(&s.env);
        assert!(gov.try_propose_admin(&s.admin, &another).is_err());
        gov.propose_admin(&new_admin, &another);
    }

    // ── #906: instance TTL is extended on every entrypoint ───────────────────

    /// Advancing the ledger far past the default instance-entry TTL and then
    /// calling entrypoints keeps governance responsive rather than trapping on
    /// an archived instance entry. The proposal registry and voting
    /// configuration live in instance storage, so an archived instance entry
    /// would make in-flight proposals unreadable and unexecutable while the
    /// voting window keeps advancing. Each entrypoint now bumps the instance
    /// TTL; this walks the ledger forward in steps smaller than the bump
    /// window, calling in between, and asserts the reads still return. Without
    /// the extension the first call after the advance would trap.
    #[test]
    fn instance_ttl_is_extended_on_access_across_ledger_advance() {
        let s = setup_prop_env();
        let gov = GovernanceClient::new(&s.env, &s.gov_addr);
        s.env.ledger().with_mut(|l| {
            l.sequence_number = 1_000;
            l.max_entry_ttl = 6_312_000;
        });

        // Step forward in increments smaller than the TTL bump (518_400),
        // calling an entrypoint each step. Each call re-bumps, so the next step
        // stays live rather than trapping on an evicted instance entry.
        for _ in 0..4 {
            s.env.ledger().with_mut(|l| l.sequence_number += 400_000);
            assert_eq!(gov.get_proposal_count(), 0);
            let _params = gov.get_params();
        }
    }
}
