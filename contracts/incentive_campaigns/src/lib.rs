#![no_std]

//! Governance-controlled trading incentive campaigns for liquidity providers.
//!
//! Supports multiple simultaneous time-based campaigns, multiple reward tokens,
//! proportional LP distribution, and a full on-chain audit trail.
//!
//! ## Reward accounting (MasterChef-style accumulator — fix for #425)
//!
//! The original implementation computed rewards as:
//!   `provider_share = reward_rate * elapsed * lp_balance / total_supply`
//!
//! That formula applies the provider's *current* LP balance retroactively to the
//! entire elapsed window since campaign start, so a late joiner who flash-deposited
//! just before claiming could steal a large share of previously accrued rewards.
//! `set_campaign_rate` had the same flaw — changing the rate rewrote the entire
//! history.
//!
//! The fix uses a **per-second accumulator** (`acc_reward_per_share`) that ticks
//! forward continuously at `reward_rate / total_supply` per second (scaled by
//! `PRECISION = 1_000_000_000_000` to preserve sub-unit precision in integer
//! arithmetic).  Each provider stores the accumulator value at their last claim
//! (`acc_at_snapshot`).  On claim:
//!
//!   `pending = lp_balance * (acc_now − acc_at_snapshot) / PRECISION`
//!
//! A flash depositor who claims in the same second as their deposit sees
//! `acc_now ≈ acc_at_snapshot` and earns essentially nothing.  An honest LP who
//! held for the full campaign window earns exactly their time-weighted share.
//!
//! `set_campaign_rate` flushes the accumulator to the current timestamp before
//! updating the rate, so past accruals are locked in at the old rate and only
//! future seconds use the new rate.

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype,
    token::Client as TokenClient, Address, Env, Symbol, Vec,
};

/// Typed failures for every caller-triggerable precondition in this contract.
///
/// Discriminants follow the workspace convention (`AlreadyInitialized = 1`,
/// `NotInitialized = 2`) and are part of the ABI: never renumber, only append.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum IncentiveError {
    /// `initialize` was called on a contract that already has governance set.
    AlreadyInitialized = 1,
    /// A function was called before `initialize`.
    NotInitialized = 2,
    /// A governance-only function was called by another address.
    Unauthorized = 3,
    /// `accept_governance` was called with no nomination outstanding.
    NoPendingGovernance = 4,
    /// `accept_governance` was called by an address other than the nominee.
    NotPendingGovernance = 5,
    /// `create_campaign` was given `end_time <= start_time`.
    InvalidCampaignWindow = 6,
    /// A reward rate was zero or negative.
    InvalidRewardRate = 7,
    /// `create_campaign` was given a zero or negative funding amount.
    InvalidFundingAmount = 8,
    /// Funding does not cover `reward_rate * (end_time - start_time)`.
    InsufficientFunding = 9,
    /// The LP token's admin is not the given pool.
    LpTokenMismatch = 10,
    /// No campaign exists with the given id.
    CampaignNotFound = 11,
    /// `recover_leftover_funds` was called before the campaign's `end_time`.
    CampaignNotEnded = 12,
    /// `recover_leftover_funds` found nothing left to recover.
    NoLeftoverFunds = 13,
    /// The campaign has been deactivated (e.g. after leftover recovery).
    CampaignInactive = 14,
    /// `claim_rewards` was called before the campaign's `start_time`.
    CampaignNotStarted = 15,
    /// The claiming provider holds no LP tokens.
    NoLpBalance = 16,
    /// The LP token has zero total supply.
    NoLpSupply = 17,
    /// The provider has no rewards accrued since their last claim.
    NoPendingRewards = 18,
    /// No distribution record exists with the given id.
    RecordNotFound = 19,
}

// ---------------------------------------------------------------------------
// LP token interface (read-only)
// ---------------------------------------------------------------------------

#[contractclient(name = "LpTokenClient")]
pub trait LpTokenInterface {
    fn balance(env: Env, id: Address) -> i128;
    fn total_supply(env: Env) -> i128;
    fn admin(env: Env) -> Address;
}

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

#[contracttype]
pub enum DataKey {
    Governance,
    /// Nominated governance address awaiting `accept_governance`.
    PendingGovernance,
    NextCampaignId,
    Campaign(u64),
    CampaignIdByIndex(u64),
    /// Per (campaign_id, provider): cumulative reward amount already accounted
    /// for by this provider's prior claims.
    ProviderDebt(u64, Address),
    /// Per (campaign_id, provider): snapshot of accumulator at last claim.
    ProviderSnapshot(u64, Address),
    /// Per campaign: total pool rewards accrued up to `CampaignLastAccrualTime`.
    CampaignAccruedRewards(u64),
    /// Per campaign: last timestamp up to which `CampaignAccruedRewards` was
    /// checkpointed. Initialized logically to `campaign.start_time`.
    CampaignLastAccrualTime(u64),
    /// Audit: next distribution record id
    NextDistributionId,
    DistributionRecord(u64),
    /// Per creator: ids of every campaign that address created, in creation order.
    CampaignsByCreator(Address),
    /// Per campaign: ids of every distribution record it produced, in claim order.
    DistributionsByCampaign(u64),
    /// Per provider: ids of every distribution record they received, in claim order.
    DistributionsByProvider(Address),
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MIN_TTL: u32 = 518_400; // ~30 days  (5 s / ledger)
const BUMP_TO: u32 = 3_110_400; // ~180 days (5 s / ledger)

/// Fixed-point scaling factor for `acc_reward_per_share`.
/// Chosen large enough that even a rate of 1 token/s with a total supply of
/// 10^14 tokens still produces a non-zero per-share increment per second.
const PRECISION: i128 = 1_000_000_000_000; // 1e12

/// Upper bound on the number of entries any paginated read may return. Keeps a
/// single read within the per-transaction resource limit however many campaigns
/// or distribution records exist.
pub const MAX_PAGE: u32 = 50;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Campaign {
    pub id: u64,
    pub pool: Address,
    pub lp_token: Address,
    pub reward_token: Address,
    pub start_time: u64,
    pub end_time: u64,
    /// Rewards emitted per second, in raw base units of `reward_token`.
    pub reward_rate: i128,
    pub active: bool,
    pub total_distributed: i128,
    pub funding_amount: i128,
    /// Global accumulator: cumulative reward per LP-share unit, scaled by PRECISION.
    /// Increases by `reward_rate * Δt / total_supply * PRECISION` each second.
    pub acc_reward_per_share: i128,
    /// Ledger timestamp at which `acc_reward_per_share` was last updated.
    pub last_update_time: u64,
}

/// Per-provider snapshot stored at each claim (replaces the old raw-claimed-amount debt).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderSnapshot {
    /// Value of `acc_reward_per_share` at the time of last claim (or campaign start
    /// for providers that have never claimed).
    pub acc_at_snapshot: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DistributionRecord {
    pub id: u64,
    pub campaign_id: u64,
    pub provider: Address,
    pub reward_token: Address,
    pub amount: i128,
    pub timestamp: u64,
}

// ---------------------------------------------------------------------------
// TTL helpers
// ---------------------------------------------------------------------------

fn extend_instance_ttl(env: &Env) {
    env.storage().instance().extend_ttl(MIN_TTL, BUMP_TO);
}

fn extend_persistent_ttl(env: &Env, key: &DataKey) {
    env.storage().persistent().extend_ttl(key, MIN_TTL, BUMP_TO);
}

// ---------------------------------------------------------------------------
// Accumulator helpers
// ---------------------------------------------------------------------------

/// Advance the campaign's `acc_reward_per_share` up to `up_to_time` and return
/// the updated campaign.  Does not write to storage — the caller must persist it.
///
/// The increment per second is:
///   `reward_rate * PRECISION / total_supply`
///
/// Integer division means very small rates (< 1 base-unit per total_supply seconds)
/// produce zero increments, which is acceptable for real-world token quantities.
fn advance_accumulator(mut campaign: Campaign, up_to_time: u64, total_supply: i128) -> Campaign {
    // Nothing to advance if supply is zero or time hasn't moved.
    if total_supply <= 0 || up_to_time <= campaign.last_update_time {
        return campaign;
    }

    let elapsed = (up_to_time - campaign.last_update_time) as i128;
    // rate * elapsed * PRECISION / total_supply
    // Multiply before divide to keep precision; overflow risk is minimal for
    // realistic token quantities (rate < 2^60, elapsed < 2^32, PRECISION = 1e12,
    // so rate*elapsed*PRECISION fits in i128 for rates up to ~10^10 tokens/s).
    let increment = campaign.reward_rate * elapsed * PRECISION / total_supply;
    campaign.acc_reward_per_share += increment;
    campaign.last_update_time = up_to_time;
    campaign
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct IncentiveCampaigns;

#[contractimpl]
impl IncentiveCampaigns {
    pub fn initialize(env: Env, governance: Address) -> Result<(), IncentiveError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(IncentiveError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&DataKey::Governance, &governance);
        env.storage()
            .instance()
            .set(&DataKey::NextCampaignId, &1u64);
        env.storage()
            .instance()
            .set(&DataKey::NextDistributionId, &1u64);
        extend_instance_ttl(&env);
        Ok(())
    }

    /// Nominate a new governance address. Current governance only.
    ///
    /// The nominee must call `accept_governance` to complete the handover, so a
    /// mistyped address cannot brick the governance-only entrypoints.
    pub fn propose_governance(
        env: Env,
        caller: Address,
        new_governance: Address,
    ) -> Result<(), IncentiveError> {
        extend_instance_ttl(&env);
        caller.require_auth();
        Self::require_governance(&env, &caller)?;

        env.storage()
            .instance()
            .set(&DataKey::PendingGovernance, &Some(new_governance.clone()));

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "governance_proposed"),),
            (caller, new_governance)
        );
        Ok(())
    }

    /// Accept a pending governance nomination. Nominee only.
    pub fn accept_governance(env: Env, new_governance: Address) -> Result<(), IncentiveError> {
        extend_instance_ttl(&env);
        let pending: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PendingGovernance)
            .unwrap_or(None);
        let nominee = pending.ok_or(IncentiveError::NoPendingGovernance)?;
        if new_governance != nominee {
            return Err(IncentiveError::NotPendingGovernance);
        }
        new_governance.require_auth();

        let old_governance = Self::read_governance(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::Governance, &new_governance);
        env.storage()
            .instance()
            .set(&DataKey::PendingGovernance, &Option::<Address>::None);

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "governance_transferred"),),
            (old_governance, new_governance)
        );
        Ok(())
    }

    /// Return the active governance address.
    pub fn get_governance(env: Env) -> Result<Address, IncentiveError> {
        Self::read_governance(&env)
    }

    /// Return the pending governance nominee, if any.
    pub fn get_pending_governance(env: Env) -> Option<Address> {
        env.storage()
            .instance()
            .get(&DataKey::PendingGovernance)
            .unwrap_or(None)
    }

    /// Create a time-based incentive campaign. Governance only.
    #[allow(clippy::too_many_arguments)]
    pub fn create_campaign(
        env: Env,
        caller: Address,
        pool: Address,
        lp_token: Address,
        reward_token: Address,
        start_time: u64,
        end_time: u64,
        reward_rate: i128,
        funding_amount: i128,
    ) -> Result<u64, IncentiveError> {
        extend_instance_ttl(&env);
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        if end_time <= start_time {
            return Err(IncentiveError::InvalidCampaignWindow);
        }
        if reward_rate <= 0 {
            return Err(IncentiveError::InvalidRewardRate);
        }
        if funding_amount <= 0 {
            return Err(IncentiveError::InvalidFundingAmount);
        }
        let duration = (end_time - start_time) as i128;
        let max_payout = reward_rate * duration;
        if funding_amount < max_payout {
            return Err(IncentiveError::InsufficientFunding);
        }

        let lp_admin = LpTokenClient::new(&env, &lp_token).admin();
        if lp_admin != pool {
            return Err(IncentiveError::LpTokenMismatch);
        }

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextCampaignId)
            .ok_or(IncentiveError::NotInitialized)?;

        let campaign = Campaign {
            id,
            pool: pool.clone(),
            lp_token: lp_token.clone(),
            reward_token: reward_token.clone(),
            start_time,
            end_time,
            reward_rate,
            active: true,
            total_distributed: 0,
            funding_amount,
            // Accumulator starts at zero; last_update_time is set to start_time so
            // that the first call to advance_accumulator uses the correct origin.
            acc_reward_per_share: 0,
            last_update_time: start_time,
        };

        let campaign_key = DataKey::Campaign(id);
        env.storage().persistent().set(&campaign_key, &campaign);
        extend_persistent_ttl(&env, &campaign_key);
        let accrued_key = DataKey::CampaignAccruedRewards(id);
        env.storage().persistent().set(&accrued_key, &0_i128);
        extend_persistent_ttl(&env, &accrued_key);
        let last_accrual_key = DataKey::CampaignLastAccrualTime(id);
        env.storage()
            .persistent()
            .set(&last_accrual_key, &start_time);
        extend_persistent_ttl(&env, &last_accrual_key);

        env.storage()
            .instance()
            .set(&DataKey::NextCampaignId, &(id + 1));

        let index = id - 1;
        let index_key = DataKey::CampaignIdByIndex(index);
        env.storage().persistent().set(&index_key, &id);
        extend_persistent_ttl(&env, &index_key);

        let creator_key = DataKey::CampaignsByCreator(caller.clone());
        let mut by_creator: Vec<u64> = env
            .storage()
            .persistent()
            .get(&creator_key)
            .unwrap_or_else(|| Vec::new(&env));
        by_creator.push_back(id);
        env.storage().persistent().set(&creator_key, &by_creator);
        extend_persistent_ttl(&env, &creator_key);

        let contract = env.current_contract_address();
        TokenClient::new(&env, &reward_token).transfer(&caller, &contract, &funding_amount);

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "campaign_created"),),
            (id, pool, reward_token, start_time, end_time, reward_rate)
        );
        Ok(id)
    }

    /// Update the reward rate for an active campaign. Governance only.
    ///
    /// The accumulator is flushed to the current timestamp *before* the rate
    /// changes, so rewards already accrued at the old rate are locked in and
    /// only future seconds use the new rate.  This prevents retroactive reward
    /// manipulation via rate changes (part of bug #425).
    pub fn set_campaign_rate(
        env: Env,
        caller: Address,
        campaign_id: u64,
        new_rate: i128,
    ) -> Result<(), IncentiveError> {
        extend_instance_ttl(&env);
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        if new_rate <= 0 {
            return Err(IncentiveError::InvalidRewardRate);
        }

        let campaign_key = DataKey::Campaign(campaign_id);
        let mut campaign: Campaign = env
            .storage()
            .persistent()
            .get(&campaign_key)
            .ok_or(IncentiveError::CampaignNotFound)?;
        extend_persistent_ttl(&env, &campaign_key);

        // Flush the payout accumulator to now *before* the rate changes, so
        // seconds already elapsed stay priced at the old rate and only future
        // seconds use `new_rate`. Without this, `claim_rewards`'s next call to
        // `advance_accumulator` would apply `new_rate` retroactively across the
        // whole interval since `last_update_time`.
        let now = env.ledger().timestamp();
        let accrual_until = Self::campaign_accrual_time(&campaign, now);
        let total_supply = LpTokenClient::new(&env, &campaign.lp_token).total_supply();
        campaign = advance_accumulator(campaign, accrual_until, total_supply);

        // Checkpoint the audit trail *before* the rate changes too, for the same
        // reason: `checkpoint_campaign_rewards` prices the whole interval since
        // `CampaignLastAccrualTime` at `campaign.reward_rate`, so it must run
        // while that field still holds the old rate.
        Self::checkpoint_campaign_rewards(&env, campaign_id, &campaign, now);

        campaign.reward_rate = new_rate;
        env.storage().persistent().set(&campaign_key, &campaign);
        extend_persistent_ttl(&env, &campaign_key);

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "rate_updated"),),
            (campaign_id, new_rate)
        );
        Ok(())
    }

    /// Recover undistributed reward tokens after a campaign has ended. Governance only.
    ///
    /// Transfers the difference between the original `funding_amount` and what was
    /// actually distributed (`total_distributed`) back to `recipient`. After recovery
    /// the campaign is marked **inactive** so no further LP claims can be made.
    ///
    /// Governance should allow a reasonable grace period after `end_time` before
    /// calling this function so that LPs have an opportunity to claim first.
    pub fn recover_leftover_funds(
        env: Env,
        caller: Address,
        campaign_id: u64,
        recipient: Address,
    ) -> Result<i128, IncentiveError> {
        extend_instance_ttl(&env);
        caller.require_auth();
        Self::require_governance(&env, &caller)?;

        let campaign_key = DataKey::Campaign(campaign_id);
        let mut campaign: Campaign = env
            .storage()
            .persistent()
            .get(&campaign_key)
            .ok_or(IncentiveError::CampaignNotFound)?;
        extend_persistent_ttl(&env, &campaign_key);

        // Recovery is the only thing that deactivates a campaign, and it leaves
        // `funding_amount - total_distributed` unchanged. Without this check a
        // second call would transfer the same leftover again, paid out of the
        // balance other campaigns hold in the same reward token.
        if !campaign.active {
            return Err(IncentiveError::CampaignInactive);
        }

        let now = env.ledger().timestamp();
        if now <= campaign.end_time {
            return Err(IncentiveError::CampaignNotEnded);
        }
        Self::checkpoint_campaign_rewards(&env, campaign_id, &campaign, now);

        let leftover = campaign.funding_amount - campaign.total_distributed;
        if leftover <= 0 {
            return Err(IncentiveError::NoLeftoverFunds);
        }

        // Mark inactive so future claim_rewards calls revert, protecting the
        // recipient from having tokens transferred twice.
        campaign.active = false;
        env.storage().persistent().set(&campaign_key, &campaign);
        extend_persistent_ttl(&env, &campaign_key);

        let contract = env.current_contract_address();
        TokenClient::new(&env, &campaign.reward_token).transfer(&contract, &recipient, &leftover);

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "leftover_recovered"),),
            (campaign_id, recipient.clone(), leftover)
        );

        Ok(leftover)
    }

    /// Distribute accrued rewards to a provider proportional to their time-weighted
    /// LP balance.
    ///
    /// ## How the accumulator model prevents retroactive gaming (#425)
    ///
    /// `acc_reward_per_share` advances at `reward_rate / total_supply` per second.
    /// Each provider stores the accumulator value at their last claim
    /// (`acc_at_snapshot`).  Pending rewards are:
    ///
    ///   `pending = lp_balance × (acc_now − acc_at_snapshot) / PRECISION`
    ///
    /// A provider who just deposited LP tokens has `acc_at_snapshot ≈ acc_now`
    /// (set the moment they first claim, which is at the earliest the next ledger),
    /// so they earn rewards only from the moment they hold LP tokens forward.
    /// They cannot reach back to the campaign start and claim rewards from a window
    /// during which they held nothing.
    pub fn claim_rewards(
        env: Env,
        provider: Address,
        campaign_id: u64,
    ) -> Result<i128, IncentiveError> {
        extend_instance_ttl(&env);
        provider.require_auth();

        let campaign_key = DataKey::Campaign(campaign_id);
        let mut campaign: Campaign = env
            .storage()
            .persistent()
            .get(&campaign_key)
            .ok_or(IncentiveError::CampaignNotFound)?;
        extend_persistent_ttl(&env, &campaign_key);
        if !campaign.active {
            return Err(IncentiveError::CampaignInactive);
        }

        let now = env.ledger().timestamp();
        if now < campaign.start_time {
            return Err(IncentiveError::CampaignNotStarted);
        }

        // Cap accrual at end_time so LPs can still claim earned rewards after the
        // campaign window closes without accruing phantom future rewards.
        let claim_time = now.min(campaign.end_time);

        let lp_balance = LpTokenClient::new(&env, &campaign.lp_token).balance(&provider);
        if lp_balance <= 0 {
            return Err(IncentiveError::NoLpBalance);
        }

        let total_supply = LpTokenClient::new(&env, &campaign.lp_token).total_supply();
        if total_supply <= 0 {
            return Err(IncentiveError::NoLpSupply);
        }

        // ── Step 2: advance campaign accumulator to current time ─────────────────

        // Update campaign accumulator based on time elapsed and total LP supply
        campaign = advance_accumulator(campaign, claim_time, total_supply);

        // Keep the parallel audit trail (`CampaignAccruedRewards` /
        // `CampaignLastAccrualTime`) in lockstep with the payout accumulator, so
        // `get_campaign_accrual` is current after a claim rather than only after
        // `set_campaign_rate` / `recover_leftover_funds`. Accrual is a pure
        // function of `reward_rate` and elapsed time, so this is a checkpoint of
        // the same interval the accumulator just advanced over.
        Self::checkpoint_campaign_rewards(&env, campaign_id, &campaign, claim_time);

        let snapshot_key = DataKey::ProviderSnapshot(campaign_id, provider.clone());
        let snapshot: Option<ProviderSnapshot> = if env.storage().persistent().has(&snapshot_key) {
            extend_persistent_ttl(&env, &snapshot_key);
            Some(env.storage().persistent().get(&snapshot_key).unwrap())
        } else {
            None
        };

        // ── Step 3: compute pending rewards ──────────────────────────────────────
        //
        // On a provider's very first interaction we initialise their snapshot to the
        // current accumulator and return 0 (nothing accrued yet for this provider).
        // Rewards start accumulating from this ledger forward.
        let pending = match snapshot {
            None => {
                // Initialise snapshot — no rewards to pay out yet.
                let init = ProviderSnapshot {
                    acc_at_snapshot: campaign.acc_reward_per_share,
                };
                env.storage().persistent().set(&snapshot_key, &init);
                extend_persistent_ttl(&env, &snapshot_key);
                // Persist the advanced accumulator even on init (no total_distributed change).
                env.storage().persistent().set(&campaign_key, &campaign);
                extend_persistent_ttl(&env, &campaign_key);
                return Ok(0);
            }
            Some(snap) => {
                let acc_delta = campaign.acc_reward_per_share - snap.acc_at_snapshot;
                let p = lp_balance * acc_delta / PRECISION;
                if p <= 0 {
                    return Err(IncentiveError::NoPendingRewards);
                }
                p
            }
        };

        // ── Step 4: pay out and persist state ────────────────────────────────────
        let contract = env.current_contract_address();
        TokenClient::new(&env, &campaign.reward_token).transfer(&contract, &provider, &pending);

        // Update provider snapshot to current accumulator.
        let new_snapshot = ProviderSnapshot {
            acc_at_snapshot: campaign.acc_reward_per_share,
        };
        env.storage().persistent().set(&snapshot_key, &new_snapshot);
        extend_persistent_ttl(&env, &snapshot_key);

        // Persist updated campaign (advanced accumulator + total_distributed).
        campaign.total_distributed += pending;
        env.storage().persistent().set(&campaign_key, &campaign);
        extend_persistent_ttl(&env, &campaign_key);

        // ── Step 5: emit audit record ─────────────────────────────────────────────
        let dist_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextDistributionId)
            .ok_or(IncentiveError::NotInitialized)?;
        let record = DistributionRecord {
            id: dist_id,
            campaign_id,
            provider: provider.clone(),
            reward_token: campaign.reward_token.clone(),
            amount: pending,
            timestamp: now,
        };
        let dist_key = DataKey::DistributionRecord(dist_id);
        env.storage().persistent().set(&dist_key, &record);
        extend_persistent_ttl(&env, &dist_key);
        env.storage()
            .instance()
            .set(&DataKey::NextDistributionId, &(dist_id + 1));

        // Index the record both ways so it can be found without already knowing
        // its id. Both lists are append-only, so they stay in claim order.
        let by_campaign_key = DataKey::DistributionsByCampaign(campaign_id);
        let mut by_campaign: Vec<u64> = env
            .storage()
            .persistent()
            .get(&by_campaign_key)
            .unwrap_or_else(|| Vec::new(&env));
        by_campaign.push_back(dist_id);
        env.storage()
            .persistent()
            .set(&by_campaign_key, &by_campaign);
        extend_persistent_ttl(&env, &by_campaign_key);

        let by_provider_key = DataKey::DistributionsByProvider(provider.clone());
        let mut by_provider: Vec<u64> = env
            .storage()
            .persistent()
            .get(&by_provider_key)
            .unwrap_or_else(|| Vec::new(&env));
        by_provider.push_back(dist_id);
        env.storage()
            .persistent()
            .set(&by_provider_key, &by_provider);
        extend_persistent_ttl(&env, &by_provider_key);

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "reward_distributed"),),
            (campaign_id, provider, pending, dist_id)
        );
        Ok(pending)
    }

    // -------------------------------------------------------------------------
    // Read-only helpers
    // -------------------------------------------------------------------------

    pub fn get_campaign(env: Env, campaign_id: u64) -> Result<Campaign, IncentiveError> {
        extend_instance_ttl(&env);
        let campaign_key = DataKey::Campaign(campaign_id);
        let campaign: Campaign = env
            .storage()
            .persistent()
            .get(&campaign_key)
            .ok_or(IncentiveError::CampaignNotFound)?;
        extend_persistent_ttl(&env, &campaign_key);
        Ok(campaign)
    }

    /// Return the campaign's accrual audit trail as
    /// `(CampaignAccruedRewards, CampaignLastAccrualTime)`.
    ///
    /// `CampaignAccruedRewards` is the total pool rewards accrued up to
    /// `CampaignLastAccrualTime`, priced at the reward rate in force over each
    /// interval. It is checkpointed by `claim_rewards`, `set_campaign_rate` and
    /// `recover_leftover_funds`, so it lags the wall clock by at most one such
    /// call; it is not recomputed on read, which keeps this a genuine read
    /// accessor.
    ///
    /// The value is a lower bound on the campaign's payouts: because the
    /// accumulator can only ever pay out rewards that have accrued,
    /// `get_campaign_accrual(id).0 >= get_campaign(id).total_distributed` holds
    /// for any sequence of calls.
    ///
    /// Returns `IncentiveError::CampaignNotFound` if the campaign does not
    /// exist, matching `get_campaign`.
    pub fn get_campaign_accrual(env: Env, campaign_id: u64) -> Result<(i128, u64), IncentiveError> {
        extend_instance_ttl(&env);
        let campaign_key = DataKey::Campaign(campaign_id);
        let campaign: Campaign = env
            .storage()
            .persistent()
            .get(&campaign_key)
            .ok_or(IncentiveError::CampaignNotFound)?;
        extend_persistent_ttl(&env, &campaign_key);

        let accrued_key = DataKey::CampaignAccruedRewards(campaign_id);
        let last_accrual_key = DataKey::CampaignLastAccrualTime(campaign_id);
        let accrued: i128 = env
            .storage()
            .persistent()
            .get(&accrued_key)
            .unwrap_or(0_i128);
        let last_accrual: u64 = env
            .storage()
            .persistent()
            .get(&last_accrual_key)
            .unwrap_or(campaign.start_time);

        // Reading extends the TTL of the entries read, consistent with every
        // other read accessor in this contract.
        extend_persistent_ttl(&env, &accrued_key);
        extend_persistent_ttl(&env, &last_accrual_key);

        Ok((accrued, last_accrual))
    }

    /// Every campaign id ever created, oldest first.
    ///
    /// **Unbounded**: this loads the whole id list and will eventually exceed
    /// the transaction resource limit. Prefer
    /// [`IncentiveCampaigns::list_campaigns_paginated`]; kept on the ABI for
    /// existing consumers.
    pub fn list_campaigns(env: Env) -> Vec<u64> {
        extend_instance_ttl(&env);
        let next_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextCampaignId)
            .unwrap_or(1);
        let count = next_id.saturating_sub(1);
        let mut all = Vec::new(&env);
        for i in 0..count {
            let key = DataKey::CampaignIdByIndex(i);
            if let Some(id) = env.storage().persistent().get::<DataKey, u64>(&key) {
                extend_persistent_ttl(&env, &key);
                all.push_back(id);
            }
        }
        all
    }

    pub fn get_campaigns(env: Env, offset: u32, limit: u32) -> Vec<u64> {
        extend_instance_ttl(&env);
        let next_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextCampaignId)
            .unwrap_or(1);
        let count = next_id.saturating_sub(1);
        let start = (offset as u64).min(count);
        let end = (start + limit as u64).min(count);

        let mut page = Vec::new(&env);
        for i in start..end {
            let key = DataKey::CampaignIdByIndex(i);
            if let Some(id) = env.storage().persistent().get::<DataKey, u64>(&key) {
                extend_persistent_ttl(&env, &key);
                page.push_back(id);
            }
        }
        page
    }

    pub fn get_distribution_record(
        env: Env,
        record_id: u64,
    ) -> Result<DistributionRecord, IncentiveError> {
        extend_instance_ttl(&env);
        let record_key = DataKey::DistributionRecord(record_id);
        let record: DistributionRecord = env
            .storage()
            .persistent()
            .get(&record_key)
            .ok_or(IncentiveError::RecordNotFound)?;
        extend_persistent_ttl(&env, &record_key);
        Ok(record)
    }

    /// Every currently active campaign.
    ///
    /// **Unbounded**: this loads the whole id list *and* deserialises every
    /// campaign behind it. Prefer
    /// [`IncentiveCampaigns::get_active_campaigns_paginated`]; kept on the ABI
    /// for existing consumers.
    pub fn get_active_campaigns(env: Env) -> Vec<Campaign> {
        extend_instance_ttl(&env);
        let ids = Self::list_campaigns(env.clone());
        let now = env.ledger().timestamp();
        let mut active: Vec<Campaign> = Vec::new(&env);
        for i in 0..ids.len() {
            let id = ids.get(i).unwrap();
            let key = DataKey::Campaign(id);
            if let Some(c) = env.storage().persistent().get::<DataKey, Campaign>(&key) {
                extend_persistent_ttl(&env, &key);
                if c.active && now >= c.start_time && now <= c.end_time {
                    active.push_back(c);
                }
            }
        }
        active
    }

    pub fn get_active_campaigns_paged(env: Env, offset: u32, limit: u32) -> Vec<Campaign> {
        extend_instance_ttl(&env);
        let next_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextCampaignId)
            .unwrap_or(1);
        let count = next_id.saturating_sub(1);
        let start = (offset as u64).min(count);
        let end = (start + limit as u64).min(count);

        let now = env.ledger().timestamp();
        let mut active: Vec<Campaign> = Vec::new(&env);
        for i in start..end {
            let idx_key = DataKey::CampaignIdByIndex(i);
            if let Some(id) = env.storage().persistent().get::<DataKey, u64>(&idx_key) {
                extend_persistent_ttl(&env, &idx_key);
                let camp_key = DataKey::Campaign(id);
                if let Some(c) = env
                    .storage()
                    .persistent()
                    .get::<DataKey, Campaign>(&camp_key)
                {
                    extend_persistent_ttl(&env, &camp_key);
                    if c.active && now >= c.start_time && now <= c.end_time {
                        active.push_back(c);
                    }
                }
            }
        }
        active
    }

    // -------------------------------------------------------------------------
    // Paginated read paths (#684)
    // -------------------------------------------------------------------------

    /// Number of campaigns ever created.
    pub fn get_campaign_count(env: Env) -> u32 {
        extend_instance_ttl(&env);
        Self::campaign_count(&env) as u32
    }

    /// Page through campaign ids in creation order.
    ///
    /// `limit` is clamped to [`MAX_PAGE`]; `limit == 0` or an `offset` at or
    /// beyond the campaign count yields an empty `Vec` rather than panicking.
    /// Paging through the whole range reproduces
    /// [`IncentiveCampaigns::list_campaigns`] exactly.
    pub fn list_campaigns_paginated(env: Env, offset: u32, limit: u32) -> Vec<u64> {
        extend_instance_ttl(&env);
        let count = Self::campaign_count(&env);
        let mut page = Vec::new(&env);
        let Some((start, end)) = Self::page_bounds(offset, limit, count) else {
            return page;
        };
        for i in start..end {
            if let Some(id) = Self::campaign_id_at(&env, i) {
                page.push_back(id);
            }
        }
        page
    }

    /// Page through full campaign structs in creation order.
    ///
    /// Same bounds as [`IncentiveCampaigns::list_campaigns_paginated`], but
    /// saves the caller a `get_campaign` round trip per id.
    pub fn get_campaigns_paginated(env: Env, offset: u32, limit: u32) -> Vec<Campaign> {
        extend_instance_ttl(&env);
        let count = Self::campaign_count(&env);
        let mut page: Vec<Campaign> = Vec::new(&env);
        let Some((start, end)) = Self::page_bounds(offset, limit, count) else {
            return page;
        };
        for i in start..end {
            if let Some(id) = Self::campaign_id_at(&env, i) {
                if let Some(c) = Self::load_campaign(&env, id) {
                    page.push_back(c);
                }
            }
        }
        page
    }

    /// Page through the currently active campaigns.
    ///
    /// `offset` and `limit` count *matching* campaigns, not scanned ones, so a
    /// page of `limit` active campaigns comes back even when inactive ones are
    /// interleaved. `limit` is clamped to [`MAX_PAGE`], which bounds the size of
    /// the result; the scan itself still walks the id list until it has filled
    /// the page, so callers paging deeply should prefer
    /// [`IncentiveCampaigns::get_campaigns_paginated`] plus their own filter.
    pub fn get_active_campaigns_paginated(env: Env, offset: u32, limit: u32) -> Vec<Campaign> {
        extend_instance_ttl(&env);
        let mut page: Vec<Campaign> = Vec::new(&env);
        let capped = limit.min(MAX_PAGE);
        if capped == 0 {
            return page;
        }

        let now = env.ledger().timestamp();
        let count = Self::campaign_count(&env);
        let mut matched: u32 = 0;
        for i in 0..count {
            let Some(id) = Self::campaign_id_at(&env, i) else {
                continue;
            };
            let Some(c) = Self::load_campaign(&env, id) else {
                continue;
            };
            if !Self::campaign_is_active(&c, now) {
                continue;
            }
            if matched >= offset {
                page.push_back(c);
                if page.len() >= capped {
                    break;
                }
            }
            matched += 1;
        }
        page
    }

    /// Page through the campaign ids created by `creator`, in creation order.
    ///
    /// Returns an empty `Vec` for an address that has never created a campaign.
    pub fn get_campaigns_by_creator(
        env: Env,
        creator: Address,
        offset: u32,
        limit: u32,
    ) -> Vec<u64> {
        extend_instance_ttl(&env);
        let key = DataKey::CampaignsByCreator(creator);
        let ids: Vec<u64> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(&env));
        Self::page_u64(&env, &ids, offset, limit)
    }

    /// Whether `campaign_id` is active right now, without pulling the struct.
    ///
    /// A campaign that does not exist is reported as inactive rather than
    /// panicking, so a caller can probe an id safely.
    pub fn is_campaign_active(env: Env, campaign_id: u64) -> bool {
        extend_instance_ttl(&env);
        match Self::load_campaign(&env, campaign_id) {
            Some(c) => Self::campaign_is_active(&c, env.ledger().timestamp()),
            None => false,
        }
    }

    /// Number of distribution records a campaign has produced.
    pub fn get_distribution_count(env: Env, campaign_id: u64) -> u32 {
        extend_instance_ttl(&env);
        Self::distribution_ids_by_campaign(&env, campaign_id).len()
    }

    /// Page through a campaign's distribution records, oldest claim first.
    pub fn list_distribution_records(
        env: Env,
        campaign_id: u64,
        offset: u32,
        limit: u32,
    ) -> Vec<DistributionRecord> {
        extend_instance_ttl(&env);
        let ids = Self::distribution_ids_by_campaign(&env, campaign_id);
        Self::page_records(&env, &ids, offset, limit)
    }

    /// Page through everything `provider` has ever claimed, in chronological
    /// order across all campaigns.
    ///
    /// Returns an empty `Vec` for a provider that has never claimed.
    pub fn get_claim_history(
        env: Env,
        provider: Address,
        offset: u32,
        limit: u32,
    ) -> Vec<DistributionRecord> {
        extend_instance_ttl(&env);
        let key = DataKey::DistributionsByProvider(provider);
        let ids: Vec<u64> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(&env));
        Self::page_records(&env, &ids, offset, limit)
    }

    // -------------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------------

    /// Resolve `(offset, limit)` against `count`, clamping `limit` to
    /// [`MAX_PAGE`]. `None` means "nothing to return": an empty limit, or an
    /// offset at or past the end.
    fn page_bounds(offset: u32, limit: u32, count: u64) -> Option<(u64, u64)> {
        let capped = limit.min(MAX_PAGE) as u64;
        if capped == 0 {
            return None;
        }
        let start = offset as u64;
        if start >= count {
            return None;
        }
        Some((start, (start + capped).min(count)))
    }

    fn campaign_count(env: &Env) -> u64 {
        let next_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextCampaignId)
            .unwrap_or(1);
        next_id.saturating_sub(1)
    }

    fn campaign_id_at(env: &Env, index: u64) -> Option<u64> {
        let key = DataKey::CampaignIdByIndex(index);
        let id = env.storage().persistent().get::<DataKey, u64>(&key)?;
        extend_persistent_ttl(env, &key);
        Some(id)
    }

    fn load_campaign(env: &Env, campaign_id: u64) -> Option<Campaign> {
        let key = DataKey::Campaign(campaign_id);
        let campaign = env.storage().persistent().get::<DataKey, Campaign>(&key)?;
        extend_persistent_ttl(env, &key);
        Some(campaign)
    }

    fn campaign_is_active(campaign: &Campaign, now: u64) -> bool {
        campaign.active && now >= campaign.start_time && now <= campaign.end_time
    }

    fn distribution_ids_by_campaign(env: &Env, campaign_id: u64) -> Vec<u64> {
        env.storage()
            .persistent()
            .get(&DataKey::DistributionsByCampaign(campaign_id))
            .unwrap_or_else(|| Vec::new(env))
    }

    /// Slice an id list with the shared pagination rules.
    fn page_u64(env: &Env, ids: &Vec<u64>, offset: u32, limit: u32) -> Vec<u64> {
        let mut page = Vec::new(env);
        let Some((start, end)) = Self::page_bounds(offset, limit, ids.len() as u64) else {
            return page;
        };
        for i in start..end {
            page.push_back(ids.get(i as u32).unwrap());
        }
        page
    }

    /// Slice a record-id list and resolve each id to its stored record.
    fn page_records(env: &Env, ids: &Vec<u64>, offset: u32, limit: u32) -> Vec<DistributionRecord> {
        let mut page: Vec<DistributionRecord> = Vec::new(env);
        let Some((start, end)) = Self::page_bounds(offset, limit, ids.len() as u64) else {
            return page;
        };
        for i in start..end {
            let record_id = ids.get(i as u32).unwrap();
            let key = DataKey::DistributionRecord(record_id);
            if let Some(record) = env
                .storage()
                .persistent()
                .get::<DataKey, DistributionRecord>(&key)
            {
                extend_persistent_ttl(env, &key);
                page.push_back(record);
            }
        }
        page
    }

    fn read_governance(env: &Env) -> Result<Address, IncentiveError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(IncentiveError::NotInitialized)
    }

    fn require_governance(env: &Env, caller: &Address) -> Result<(), IncentiveError> {
        if caller != &Self::read_governance(env)? {
            return Err(IncentiveError::Unauthorized);
        }
        Ok(())
    }

    fn campaign_accrual_time(campaign: &Campaign, now: u64) -> u64 {
        if now <= campaign.start_time {
            campaign.start_time
        } else if now >= campaign.end_time {
            campaign.end_time
        } else {
            now
        }
    }

    fn checkpoint_campaign_rewards(
        env: &Env,
        campaign_id: u64,
        campaign: &Campaign,
        now: u64,
    ) -> i128 {
        let accrued_key = DataKey::CampaignAccruedRewards(campaign_id);
        let last_accrual_key = DataKey::CampaignLastAccrualTime(campaign_id);

        let mut accrued: i128 = env
            .storage()
            .persistent()
            .get(&accrued_key)
            .unwrap_or(0_i128);
        let mut last_accrual: u64 = env
            .storage()
            .persistent()
            .get(&last_accrual_key)
            .unwrap_or(campaign.start_time);

        let accrual_until = Self::campaign_accrual_time(campaign, now);
        if last_accrual < campaign.start_time {
            last_accrual = campaign.start_time;
        } else if last_accrual > campaign.end_time {
            last_accrual = campaign.end_time;
        }

        if accrual_until > last_accrual {
            let elapsed = (accrual_until - last_accrual) as i128;
            accrued += campaign.reward_rate * elapsed;
            last_accrual = accrual_until;
        }

        env.storage().persistent().set(&accrued_key, &accrued);
        extend_persistent_ttl(env, &accrued_key);
        env.storage()
            .persistent()
            .set(&last_accrual_key, &last_accrual);
        extend_persistent_ttl(env, &last_accrual_key);
        accrued
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amm::{AmmPool, AmmPoolClient};
    use soroban_sdk::{
        testutils::{storage::Persistent as _, Address as _, Ledger},
        token::StellarAssetClient,
        Address, Env,
    };
    use token::{LpToken, LpTokenClient};

    // -------------------------------------------------------------------------
    // Test harness
    // -------------------------------------------------------------------------

    /// Returns (env, incentives_addr, amm_addr, lp_addr, reward_addr, provider, gov).
    ///
    /// Pool state after setup:
    ///   - provider deposited 1_000_000 / 1_000_000 into the AMM.
    ///   - AMM locks MINIMUM_LIQUIDITY = 1_000, so total_supply = 1_000_000 and
    ///     provider holds 999_000 shares (999_000 / 1_000_000 = 99.9 %).
    ///   - gov holds 10_000_000 reward tokens.
    ///   - Ledger timestamp is 1_000.
    fn setup() -> (Env, Address, Address, Address, Address, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);

        let gov = Address::generate(&env);
        let provider = Address::generate(&env);
        let admin = Address::generate(&env);

        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "LP"),
            &soroban_sdk::String::from_str(&env, "LP"),
            &7u32,
        );

        let ta = env.register_stellar_asset_contract_v2(admin.clone());
        let tb = env.register_stellar_asset_contract_v2(admin.clone());
        let reward = env.register_stellar_asset_contract_v2(admin.clone());

        AmmPoolClient::new(&env, &amm_addr).initialize(
            &admin,
            &ta.address(),
            &tb.address(),
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        StellarAssetClient::new(&env, &ta.address()).mint(&provider, &1_000_000);
        StellarAssetClient::new(&env, &tb.address()).mint(&provider, &1_000_000);
        AmmPoolClient::new(&env, &amm_addr).add_liquidity(
            &provider,
            &1_000_000,
            &1_000_000,
            &0,
            &u64::MAX,
        );

        StellarAssetClient::new(&env, &reward.address()).mint(&gov, &10_000_000);

        let incentives = env.register_contract(None, IncentiveCampaigns);
        IncentiveCampaignsClient::new(&env, &incentives).initialize(&gov);

        (
            env,
            incentives,
            amm_addr,
            lp_addr,
            reward.address(),
            provider,
            gov,
        )
    }

    // -------------------------------------------------------------------------
    // Bug #425 regression: flash-deposit gaming
    // -------------------------------------------------------------------------

    /// A late joiner who deposits LP tokens *after* a campaign has been running
    /// for some time should earn rewards only from the moment they first claim,
    /// not retroactively from campaign start.
    #[test]
    fn test_flash_deposit_cannot_claim_retroactive_rewards() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);

        let gov = Address::generate(&env);
        let honest_provider = Address::generate(&env);
        let late_joiner = Address::generate(&env);
        let admin = Address::generate(&env);

        // Set up AMM + LP token
        let amm_addr = env.register_contract(None, AmmPool);
        let lp_addr = env.register_contract(None, LpToken);
        LpTokenClient::new(&env, &lp_addr).initialize(
            &amm_addr,
            &soroban_sdk::String::from_str(&env, "LP"),
            &soroban_sdk::String::from_str(&env, "LP"),
            &7u32,
        );
        let ta = env.register_stellar_asset_contract_v2(admin.clone());
        let tb = env.register_stellar_asset_contract_v2(admin.clone());
        let reward = env.register_stellar_asset_contract_v2(admin.clone());
        AmmPoolClient::new(&env, &amm_addr).initialize(
            &admin,
            &ta.address(),
            &tb.address(),
            &lp_addr,
            &30_i128,
            &admin,
            &0_i128,
        );

        // Honest provider deposits at t=1_000 (campaign start).
        StellarAssetClient::new(&env, &ta.address()).mint(&honest_provider, &2_000_000);
        StellarAssetClient::new(&env, &tb.address()).mint(&honest_provider, &2_000_000);
        AmmPoolClient::new(&env, &amm_addr).add_liquidity(
            &honest_provider,
            &2_000_000,
            &2_000_000,
            &0,
            &u64::MAX,
        );

        StellarAssetClient::new(&env, &reward.address()).mint(&gov, &10_000_000);

        let incentives = env.register_contract(None, IncentiveCampaigns);
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        client.initialize(&gov);

        // Campaign: t=1_000..11_000, rate=100 tokens/s.
        let id = client.create_campaign(
            &gov,
            &amm_addr,
            &lp_addr,
            &reward.address(),
            &1_000,
            &11_000,
            &100,
            &1_000_000,
        );

        // ── Honest provider establishes snapshot at t=1_000 ──────────────────────
        // First call initialises the snapshot → returns 0 (acc delta = 0, nothing accrued yet).
        assert_eq!(
            client.claim_rewards(&honest_provider, &id),
            0,
            "first claim at t=start should return 0 (snapshot init)"
        );

        // ── Late joiner deposits at t=6_000 (halfway through) ────────────────────
        env.ledger().with_mut(|l| l.timestamp = 6_000);
        StellarAssetClient::new(&env, &ta.address()).mint(&late_joiner, &1_000_000);
        StellarAssetClient::new(&env, &tb.address()).mint(&late_joiner, &1_000_000);
        AmmPoolClient::new(&env, &amm_addr).add_liquidity(
            &late_joiner,
            &1_000_000,
            &1_000_000,
            &0,
            &u64::MAX,
        );

        // Late joiner's first claim initialises their snapshot at t=6_000 → returns 0.
        // They cannot claim rewards for t=1_000..6_000 that accrued before they joined.
        assert_eq!(
            client.claim_rewards(&late_joiner, &id),
            0,
            "late joiner snapshot init should return 0"
        );

        // ── Advance to t=11_000 (end of campaign) ────────────────────────────────
        env.ledger().with_mut(|l| l.timestamp = 11_000);

        let late_claimed = client.claim_rewards(&late_joiner, &id);
        let honest_claimed = client.claim_rewards(&honest_provider, &id);

        // Late joiner held LP tokens from t=6_000..11_000 (5_000 s).
        // Honest provider held from t=1_000..11_000 (10_000 s, snapshot at t=1_000).
        // Therefore honest_claimed > late_claimed.
        assert!(
            honest_claimed > late_claimed,
            "honest provider (full duration) must earn more than late joiner (half duration): \
             honest={honest_claimed}, late={late_claimed}"
        );

        // The late joiner must earn strictly more than zero (they held for 5_000 s).
        assert!(
            late_claimed > 0,
            "late joiner should earn something for t=6_000..11_000"
        );

        // Key invariant: the late joiner must earn LESS than they would have if they
        // had held the full campaign (honest provider held twice as long and with a
        // larger share, so their reward must be significantly higher).
        // In the old buggy code a late joiner could claim as much as or more than
        // an honest LP who held from the start.  Under the accumulator model the
        // late joiner only earns rewards accrued since their snapshot at t=6_000.
        assert!(
            honest_claimed > late_claimed * 2,
            "honest provider should earn more than 2× late joiner (held longer + larger share): \
             honest={honest_claimed}, late={late_claimed}"
        );
    }

    // -------------------------------------------------------------------------
    // Bug #425 regression: rate-change retroactivity
    // -------------------------------------------------------------------------

    /// Changing the rate via `set_campaign_rate` must not retroactively alter
    /// rewards already accrued at the old rate.
    #[test]
    fn test_rate_change_is_not_retroactive() {
        let (env, incentives, _pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        // Campaign: t=1_000..20_000, rate=100.
        let id = client.create_campaign(
            &gov, &_pool, &lp, &reward, &1_000, &20_000, &100, &2_000_000,
        );

        // Establish provider snapshot at t=1_000 (returns 0, nothing accrued yet).
        assert_eq!(client.claim_rewards(&provider, &id), 0);

        // Advance to t=6_000 and claim: should cover 5_000 s at rate=100.
        env.ledger().with_mut(|l| l.timestamp = 6_000);
        let claim1 = client.claim_rewards(&provider, &id);

        // Now governance increases the rate to 200 at t=6_000.
        client.set_campaign_rate(&gov, &id, &200);

        // Advance to t=11_000 and claim: should cover 5_000 s at rate=200, not 100.
        env.ledger().with_mut(|l| l.timestamp = 11_000);
        let claim2 = client.claim_rewards(&provider, &id);

        // At rate=200, the second window earns ~2× the first window (same duration, same supply).
        // Accept ±1 due to integer rounding.
        assert!(
            claim2 >= claim1 * 2 - 2 && claim2 <= claim1 * 2 + 2,
            "second claim (rate=200) should be ~2× first claim (rate=100): \
             claim1={claim1}, claim2={claim2}"
        );
    }

    // -------------------------------------------------------------------------
    // Existing behaviour tests (updated for accumulator math)
    // -------------------------------------------------------------------------

    #[test]
    fn test_multiple_campaigns_and_distribution_audit() {
        let (env, incentives, pool, lp, reward, provider, gov_addr) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id1 = client.create_campaign(
            &gov_addr, &pool, &lp, &reward, &1_000, &10_000, &100, &1_000_000,
        );
        let id2 = client.create_campaign(
            &gov_addr, &pool, &lp, &reward, &1_000, &20_000, &50, &1_000_000,
        );
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(client.list_campaigns().len(), 2);

        // Establish snapshots at t=1_000 (returns 0, nothing accrued yet).
        assert_eq!(client.claim_rewards(&provider, &id1), 0);
        assert_eq!(client.claim_rewards(&provider, &id2), 0);

        // Advance and claim.
        env.ledger().with_mut(|l| l.timestamp = 2_000);
        let claimed = client.claim_rewards(&provider, &id1);
        assert!(claimed > 0);

        let record = client.get_distribution_record(&1);
        assert_eq!(record.campaign_id, id1);
        assert_eq!(record.provider, provider);
    }

    #[test]
    fn test_claim_after_end_time() {
        let (env, incentives, pool, lp, reward, provider, gov_addr) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        // Campaign: t=1_000..5_000, rate=100.
        let id = client.create_campaign(
            &gov_addr, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000,
        );

        // ── Establish snapshot at campaign start ──────────────────────────────────
        // First call at t=1_000: returns 0 (snapshot init, no time has elapsed yet).
        assert_eq!(
            client.claim_rewards(&provider, &id),
            0,
            "snapshot init at campaign start should return 0"
        );

        // ── Claim after end_time ──────────────────────────────────────────────────
        env.ledger().with_mut(|l| l.timestamp = 8_000);
        let claimed_after_end = client.claim_rewards(&provider, &id);
        assert!(
            claimed_after_end > 0,
            "expected non-zero rewards after end_time"
        );

        // Elapsed window = end_time − start_time = 4_000 s.
        // acc_increment = rate * elapsed * PRECISION / total_supply
        //               = 100 * 4_000 * 1e12 / 1_000_000 = 4e11
        // pending = lp_balance * acc_delta / PRECISION
        //         = 999_000 * 4e11 / 1e12 = 399_600
        assert_eq!(
            claimed_after_end, 399_600,
            "rewards must be capped at end_time"
        );

        // Duplicate claim must fail.
        assert_eq!(
            client.try_claim_rewards(&provider, &id),
            Err(Ok(IncentiveError::NoPendingRewards)),
            "second claim should fail with NoPendingRewards"
        );

        // ── Partial claim then remainder after end ────────────────────────────────
        // Use a campaign whose window is entirely in the future relative to now (t=8_000).
        // Campaign 2: t=10_000..20_000, rate=100.
        let id2 = client.create_campaign(
            &gov_addr, &pool, &lp, &reward, &10_000, &20_000, &100, &1_000_000,
        );

        // Establish snapshot at t=10_000 (campaign start).  Returns 0 — nothing accrued yet.
        env.ledger().with_mut(|l| l.timestamp = 10_000);
        assert_eq!(
            client.claim_rewards(&provider, &id2),
            0,
            "snapshot init should return 0"
        );

        // Partial claim at t=12_000 (2_000 s after snapshot).
        env.ledger().with_mut(|l| l.timestamp = 12_000);
        let partial = client.claim_rewards(&provider, &id2);
        // 100 * 2_000 * 1e12 / 1_000_000 = 2e8; 999_000 * 2e8 / 1e12 = 199_800
        assert_eq!(partial, 199_800, "partial claim t=10_000..12_000");

        // Second claim well after end_time=20_000; should yield the remaining 8_000 s.
        env.ledger().with_mut(|l| l.timestamp = 25_000);
        let remainder = client.claim_rewards(&provider, &id2);
        // 100 * 8_000 * 1e12 / 1_000_000 = 8e8; 999_000 * 8e8 / 1e12 = 799_200
        assert_eq!(remainder, 799_200, "remainder claim t=12_000..20_000");

        assert_eq!(
            client.try_claim_rewards(&provider, &id2),
            Err(Ok(IncentiveError::NoPendingRewards)),
            "third claim should fail"
        );
    }

    #[test]
    fn test_recover_leftover_funds() {
        let (env, incentives, pool, lp, reward, provider, gov_addr) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let treasury = Address::generate(&env);

        // Campaign: t=1_000..5_000, rate=100, funding=1_000_000.
        let id = client.create_campaign(
            &gov_addr, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000,
        );

        // Establish provider snapshot at t=1_000 (returns 0, nothing accrued yet).
        assert_eq!(client.claim_rewards(&provider, &id), 0);

        // Partial claim at t=2_000 (1_000 s elapsed since snapshot).
        env.ledger().with_mut(|l| l.timestamp = 2_000);
        let claimed = client.claim_rewards(&provider, &id);
        // 100 * 1_000 * 1e12 / 1_000_000 = 1e8; 999_000 * 1e8 / 1e12 = 99_900
        assert_eq!(claimed, 99_900);

        // Advance past end_time and recover leftover.
        env.ledger().with_mut(|l| l.timestamp = 8_000);
        let recovered = client.recover_leftover_funds(&gov_addr, &id, &treasury);
        assert_eq!(
            recovered,
            1_000_000 - 99_900,
            "should recover funding minus distributed"
        );

        // Post-recovery claims must fail.
        assert_eq!(
            client.try_claim_rewards(&provider, &id),
            Err(Ok(IncentiveError::CampaignInactive)),
            "claim after recovery must fail (campaign inactive)"
        );

        // ── No claims at all → full funding recovered ─────────────────────────────
        let id2 = client.create_campaign(
            &gov_addr, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000,
        );
        env.ledger().with_mut(|l| l.timestamp = 9_000);
        let full_recovery = client.recover_leftover_funds(&gov_addr, &id2, &treasury);
        assert_eq!(full_recovery, 1_000_000);

        // ── Recovery before end_time must be rejected ─────────────────────────────
        let id3 = client.create_campaign(
            &gov_addr, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000,
        );
        env.ledger().with_mut(|l| l.timestamp = 3_000);
        assert_eq!(
            client.try_recover_leftover_funds(&gov_addr, &id3, &treasury),
            Err(Ok(IncentiveError::CampaignNotEnded)),
            "recovery before end_time must be rejected"
        );
    }

    #[test]
    fn test_paged_campaigns_and_active_paged() {
        let (env, incentives, pool, lp, reward, _provider, gov_addr) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        let id1 = client.create_campaign(
            &gov_addr, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000,
        );
        let id2 = client.create_campaign(
            &gov_addr, &pool, &lp, &reward, &1_000, &10_000, &100, &1_000_000,
        );
        let id3 = client.create_campaign(
            &gov_addr, &pool, &lp, &reward, &4_000, &10_000, &100, &1_000_000,
        );

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);

        let page_1_2 = client.get_campaigns(&0, &2);
        assert_eq!(page_1_2.len(), 2);
        assert_eq!(page_1_2.get(0).unwrap(), 1);
        assert_eq!(page_1_2.get(1).unwrap(), 2);

        let page_3 = client.get_campaigns(&2, &2);
        assert_eq!(page_3.len(), 1);
        assert_eq!(page_3.get(0).unwrap(), 3);

        env.ledger().with_mut(|l| l.timestamp = 2_000);
        let active_page_1 = client.get_active_campaigns_paged(&0, &2);
        assert_eq!(active_page_1.len(), 2);
        assert_eq!(active_page_1.get(0).unwrap().id, 1);
        assert_eq!(active_page_1.get(1).unwrap().id, 2);

        let active_page_2 = client.get_active_campaigns_paged(&2, &2);
        assert_eq!(active_page_2.len(), 0);
    }

    #[test]
    fn test_rate_increase_checkpoints_prior_accrual() {
        let (env, incentives, pool, lp, reward, provider, gov_addr) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = client.create_campaign(
            &gov_addr, &pool, &lp, &reward, &1_000, &5_000, &100, &2_000_000,
        );

        // First call initialises the provider's snapshot at campaign start and
        // returns 0 (nothing accrued yet) — see `claim_rewards` docs.
        assert_eq!(client.claim_rewards(&provider, &id), 0);

        env.ledger().with_mut(|l| l.timestamp = 2_000);
        let first_claim = client.claim_rewards(&provider, &id);
        assert_eq!(first_claim, 99_900);

        env.ledger().with_mut(|l| l.timestamp = 3_000);
        client.set_campaign_rate(&gov_addr, &id, &200);

        env.ledger().with_mut(|l| l.timestamp = 4_000);
        let second_claim = client.claim_rewards(&provider, &id);
        // Piecewise accrual:
        // t=1_000..3_000 @ 100 = 200_000 pool rewards
        // t=3_000..4_000 @ 200 = 200_000 pool rewards
        // Provider owns 999_000 / 1_000_000 of LP supply => cumulative 399_600.
        // After the first 99_900 claim, only 299_700 remains.
        assert_eq!(second_claim, 299_700);
    }

    #[test]
    fn test_rate_decrease_does_not_block_future_claims() {
        let (env, incentives, pool, lp, reward, provider, gov_addr) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = client.create_campaign(
            &gov_addr, &pool, &lp, &reward, &1_000, &5_000, &200, &2_000_000,
        );

        // First call initialises the provider's snapshot at campaign start and
        // returns 0 (nothing accrued yet) — see `claim_rewards` docs.
        assert_eq!(client.claim_rewards(&provider, &id), 0);

        env.ledger().with_mut(|l| l.timestamp = 3_000);
        let first_claim = client.claim_rewards(&provider, &id);
        assert_eq!(first_claim, 399_600);

        env.ledger().with_mut(|l| l.timestamp = 3_000);
        client.set_campaign_rate(&gov_addr, &id, &100);

        env.ledger().with_mut(|l| l.timestamp = 4_000);
        let second_claim = client.claim_rewards(&provider, &id);
        // Piecewise accrual:
        // t=1_000..3_000 @ 200 = 400_000 pool rewards
        // t=3_000..4_000 @ 100 = 100_000 pool rewards
        // Provider's cumulative share is 499_500, so 99_900 remains claimable.
        assert_eq!(second_claim, 99_900);
    }

    // -------------------------------------------------------------------------
    // Bug #548: persistent campaign entries must bump TTL (like pol_vesting)
    // -------------------------------------------------------------------------

    /// Creating a campaign, claiming rewards, and reading audit records must
    /// extend persistent TTLs so long-running campaigns are not archived.
    #[test]
    fn test_persistent_entries_extend_ttl_on_write_and_read() {
        let (env, incentives, pool, lp, reward, provider, gov_addr) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        let id = client.create_campaign(
            &gov_addr, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000,
        );

        // After create: Campaign, index, and accrual keys must be bumped.
        env.as_contract(&incentives, || {
            let campaign_ttl = env.storage().persistent().get_ttl(&DataKey::Campaign(id));
            let index_ttl = env
                .storage()
                .persistent()
                .get_ttl(&DataKey::CampaignIdByIndex(id - 1));
            let accrued_ttl = env
                .storage()
                .persistent()
                .get_ttl(&DataKey::CampaignAccruedRewards(id));
            let last_accrual_ttl = env
                .storage()
                .persistent()
                .get_ttl(&DataKey::CampaignLastAccrualTime(id));
            assert!(
                campaign_ttl >= BUMP_TO - 1,
                "Campaign TTL {campaign_ttl} should be bumped toward BUMP_TO"
            );
            assert!(
                index_ttl >= BUMP_TO - 1,
                "CampaignIdByIndex TTL {index_ttl} should be bumped toward BUMP_TO"
            );
            assert!(
                accrued_ttl >= BUMP_TO - 1,
                "CampaignAccruedRewards TTL {accrued_ttl} should be bumped toward BUMP_TO"
            );
            assert!(
                last_accrual_ttl >= BUMP_TO - 1,
                "CampaignLastAccrualTime TTL {last_accrual_ttl} should be bumped toward BUMP_TO"
            );
        });

        // Claim once to init snapshot (returns 0), then claim again for a payout
        // so a DistributionRecord is written.
        env.ledger().with_mut(|l| l.timestamp = 2_000);
        assert_eq!(client.claim_rewards(&provider, &id), 0);
        env.ledger().with_mut(|l| l.timestamp = 3_000);
        let paid = client.claim_rewards(&provider, &id);
        assert!(paid > 0);

        env.as_contract(&incentives, || {
            let snapshot_ttl = env
                .storage()
                .persistent()
                .get_ttl(&DataKey::ProviderSnapshot(id, provider.clone()));
            let dist_ttl = env
                .storage()
                .persistent()
                .get_ttl(&DataKey::DistributionRecord(1));
            assert!(
                snapshot_ttl >= BUMP_TO - 1,
                "ProviderSnapshot TTL {snapshot_ttl} should be bumped toward BUMP_TO"
            );
            assert!(
                dist_ttl >= BUMP_TO - 1,
                "DistributionRecord TTL {dist_ttl} should be bumped toward BUMP_TO"
            );
        });

        // Read paths must also refresh TTL (get_campaign / get_distribution_record).
        let _ = client.get_campaign(&id);
        let _ = client.get_distribution_record(&1);
        let _ = client.get_active_campaigns();

        env.as_contract(&incentives, || {
            let campaign_ttl = env.storage().persistent().get_ttl(&DataKey::Campaign(id));
            assert!(
                campaign_ttl >= BUMP_TO - 1,
                "get_campaign must refresh Campaign TTL, got {campaign_ttl}"
            );
        });
    }

    // -------------------------------------------------------------------------
    // #684: pagination, creator index, distribution-record indices
    // -------------------------------------------------------------------------

    /// Create `n` campaigns owned by `gov`, all running over the same window,
    /// and return their ids in creation order.
    fn seed_campaigns(
        client: &IncentiveCampaignsClient,
        gov: &Address,
        pool: &Address,
        lp: &Address,
        reward: &Address,
        n: u32,
    ) -> soroban_sdk::Vec<u64> {
        let mut ids = soroban_sdk::Vec::new(&client.env);
        for _ in 0..n {
            ids.push_back(
                client.create_campaign(gov, pool, lp, reward, &1_000, &11_000, &1, &10_000),
            );
        }
        ids
    }

    #[test]
    fn test_campaign_count_tracks_creations() {
        let (env, incentives, amm, lp, reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        assert_eq!(client.get_campaign_count(), 0);

        seed_campaigns(&client, &gov, &amm, &lp, &reward, 3);
        assert_eq!(client.get_campaign_count(), 3);
    }

    #[test]
    fn test_paging_in_threes_reproduces_list_campaigns() {
        let (env, incentives, amm, lp, reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        seed_campaigns(&client, &gov, &amm, &lp, &reward, 7);

        let all = client.list_campaigns();
        let mut paged: soroban_sdk::Vec<u64> = soroban_sdk::Vec::new(&env);
        let mut offset = 0u32;
        loop {
            let page = client.list_campaigns_paginated(&offset, &3);
            if page.is_empty() {
                break;
            }
            for id in page.iter() {
                paged.push_back(id);
            }
            offset += 3;
        }
        assert_eq!(paged, all);
    }

    #[test]
    fn test_list_campaigns_paginated_boundaries() {
        let (env, incentives, amm, lp, reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        seed_campaigns(&client, &gov, &amm, &lp, &reward, 4);

        // limit == 0 returns an empty page.
        assert_eq!(client.list_campaigns_paginated(&0, &0).len(), 0);
        // offset == count and offset > count return empty, not a panic.
        assert_eq!(client.list_campaigns_paginated(&4, &10).len(), 0);
        assert_eq!(client.list_campaigns_paginated(&99, &10).len(), 0);
        // A partial trailing page is truncated to what remains.
        assert_eq!(client.list_campaigns_paginated(&3, &10).len(), 1);
        // limit > MAX_PAGE is clamped, not rejected.
        assert_eq!(
            client
                .list_campaigns_paginated(&0, &(MAX_PAGE + 1_000))
                .len(),
            4
        );
    }

    #[test]
    fn test_get_campaigns_paginated_returns_structs_in_order() {
        let (env, incentives, amm, lp, reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let ids = seed_campaigns(&client, &gov, &amm, &lp, &reward, 5);

        let page = client.get_campaigns_paginated(&1, &2);
        assert_eq!(page.len(), 2);
        assert_eq!(page.get(0).unwrap().id, ids.get(1).unwrap());
        assert_eq!(page.get(1).unwrap().id, ids.get(2).unwrap());
        assert_eq!(page.get(0).unwrap().pool, amm);
    }

    #[test]
    fn test_active_pagination_skips_interleaved_inactive_campaigns() {
        let (env, incentives, amm, lp, reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        // Interleave expired campaigns (window already closed at t = 1_000)
        // with live ones so filtering has to skip over them.
        let mut active_ids: soroban_sdk::Vec<u64> = soroban_sdk::Vec::new(&env);
        for _ in 0..5 {
            client.create_campaign(&gov, &amm, &lp, &reward, &10, &100, &1, &1_000);
            active_ids.push_back(
                client.create_campaign(&gov, &amm, &lp, &reward, &1_000, &11_000, &1, &10_000),
            );
        }

        let page = client.get_active_campaigns_paginated(&0, &5);
        assert_eq!(page.len(), 5);
        for i in 0..5u32 {
            assert_eq!(page.get(i).unwrap().id, active_ids.get(i).unwrap());
        }

        // The unbounded view agrees on the same set.
        assert_eq!(client.get_active_campaigns().len(), 5);

        // Offsets count matching campaigns, not scanned ones.
        let tail = client.get_active_campaigns_paginated(&3, &10);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail.get(0).unwrap().id, active_ids.get(3).unwrap());

        // Boundaries behave like every other paginated read.
        assert_eq!(client.get_active_campaigns_paginated(&0, &0).len(), 0);
        assert_eq!(client.get_active_campaigns_paginated(&5, &5).len(), 0);
        assert_eq!(
            client
                .get_active_campaigns_paginated(&0, &(MAX_PAGE + 1_000))
                .len(),
            5
        );
    }

    #[test]
    fn test_is_campaign_active_reflects_the_window() {
        let (env, incentives, amm, lp, reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        let live = client.create_campaign(&gov, &amm, &lp, &reward, &1_000, &11_000, &1, &10_000);
        let expired = client.create_campaign(&gov, &amm, &lp, &reward, &10, &100, &1, &1_000);

        assert!(client.is_campaign_active(&live));
        assert!(!client.is_campaign_active(&expired));
        // An id that was never created is inactive, not a panic.
        assert!(!client.is_campaign_active(&9_999));

        env.ledger().with_mut(|l| l.timestamp = 20_000);
        assert!(!client.is_campaign_active(&live));
    }

    #[test]
    fn test_campaigns_by_creator_is_scoped_to_that_creator() {
        let (env, incentives, amm, lp, reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let ids = seed_campaigns(&client, &gov, &amm, &lp, &reward, 3);

        let mine = client.get_campaigns_by_creator(&gov, &0, &10);
        assert_eq!(mine, ids);

        // Hand governance to a second address and let it create one campaign.
        let other = Address::generate(&env);
        StellarAssetClient::new(&env, &reward).mint(&other, &10_000_000);
        client.propose_governance(&gov, &other);
        client.accept_governance(&other);
        let other_id =
            client.create_campaign(&other, &amm, &lp, &reward, &1_000, &11_000, &1, &10_000);

        assert_eq!(client.get_campaigns_by_creator(&gov, &0, &10), ids);
        assert_eq!(
            client.get_campaigns_by_creator(&other, &0, &10),
            soroban_sdk::vec![&env, other_id]
        );
    }

    #[test]
    fn test_campaigns_by_creator_pagination_and_empty_creator() {
        let (env, incentives, amm, lp, reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let ids = seed_campaigns(&client, &gov, &amm, &lp, &reward, 4);

        let page = client.get_campaigns_by_creator(&gov, &2, &1);
        assert_eq!(page, soroban_sdk::vec![&env, ids.get(2).unwrap()]);
        assert_eq!(client.get_campaigns_by_creator(&gov, &0, &0).len(), 0);
        assert_eq!(client.get_campaigns_by_creator(&gov, &4, &10).len(), 0);
        assert_eq!(
            client
                .get_campaigns_by_creator(&gov, &0, &(MAX_PAGE + 1_000))
                .len(),
            4
        );

        // A creator that has never created a campaign gets an empty Vec.
        let stranger = Address::generate(&env);
        assert_eq!(client.get_campaigns_by_creator(&stranger, &0, &10).len(), 0);
    }

    /// Claim twice so the provider builds up a two-record history, and return
    /// the campaign id.
    fn claim_twice(
        env: &Env,
        client: &IncentiveCampaignsClient,
        gov: &Address,
        amm: &Address,
        lp: &Address,
        reward: &Address,
        provider: &Address,
    ) -> u64 {
        let id = client.create_campaign(gov, amm, lp, reward, &1_000, &11_000, &100, &1_000_000);
        // First call only initialises the provider snapshot.
        client.claim_rewards(provider, &id);
        env.ledger().with_mut(|l| l.timestamp = 3_000);
        client.claim_rewards(provider, &id);
        env.ledger().with_mut(|l| l.timestamp = 5_000);
        client.claim_rewards(provider, &id);
        id
    }

    #[test]
    fn test_distribution_records_are_indexed_per_campaign() {
        let (env, incentives, amm, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = claim_twice(&env, &client, &gov, &amm, &lp, &reward, &provider);

        assert_eq!(client.get_distribution_count(&id), 2);
        let records = client.list_distribution_records(&id, &0, &10);
        assert_eq!(records.len(), 2);
        assert_eq!(records.get(0).unwrap().campaign_id, id);
        assert_eq!(records.get(0).unwrap().provider, provider);
        // Chronological order.
        assert!(records.get(0).unwrap().timestamp < records.get(1).unwrap().timestamp);
        // Each record round-trips through the by-id getter.
        let first_id = records.get(0).unwrap().id;
        assert_eq!(
            client.get_distribution_record(&first_id),
            records.get(0).unwrap()
        );
    }

    #[test]
    fn test_distribution_record_pagination_boundaries() {
        let (env, incentives, amm, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = claim_twice(&env, &client, &gov, &amm, &lp, &reward, &provider);

        assert_eq!(client.list_distribution_records(&id, &0, &0).len(), 0);
        assert_eq!(client.list_distribution_records(&id, &1, &10).len(), 1);
        assert_eq!(client.list_distribution_records(&id, &2, &10).len(), 0);
        assert_eq!(
            client
                .list_distribution_records(&id, &0, &(MAX_PAGE + 1_000))
                .len(),
            2
        );
        // A campaign nobody has claimed from has no records.
        let empty = client.create_campaign(&gov, &amm, &lp, &reward, &1_000, &11_000, &1, &10_000);
        assert_eq!(client.get_distribution_count(&empty), 0);
        assert_eq!(client.list_distribution_records(&empty, &0, &10).len(), 0);
    }

    #[test]
    fn test_claim_history_is_chronological_across_campaigns() {
        let (env, incentives, amm, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let first = claim_twice(&env, &client, &gov, &amm, &lp, &reward, &provider);

        // A second campaign the same provider also claims from.
        let second =
            client.create_campaign(&gov, &amm, &lp, &reward, &5_000, &11_000, &100, &600_000);
        client.claim_rewards(&provider, &second);
        env.ledger().with_mut(|l| l.timestamp = 7_000);
        client.claim_rewards(&provider, &second);

        let history = client.get_claim_history(&provider, &0, &10);
        assert_eq!(history.len(), 3);
        assert_eq!(history.get(0).unwrap().campaign_id, first);
        assert_eq!(history.get(2).unwrap().campaign_id, second);
        for i in 0..history.len() - 1 {
            assert!(history.get(i).unwrap().timestamp <= history.get(i + 1).unwrap().timestamp);
            assert!(history.get(i).unwrap().id < history.get(i + 1).unwrap().id);
        }
    }

    #[test]
    fn test_claim_history_pagination_and_provider_with_no_claims() {
        let (env, incentives, amm, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        claim_twice(&env, &client, &gov, &amm, &lp, &reward, &provider);

        assert_eq!(client.get_claim_history(&provider, &0, &1).len(), 1);
        assert_eq!(client.get_claim_history(&provider, &0, &0).len(), 0);
        assert_eq!(client.get_claim_history(&provider, &2, &10).len(), 0);
        assert_eq!(
            client
                .get_claim_history(&provider, &0, &(MAX_PAGE + 1_000))
                .len(),
            2
        );

        let stranger = Address::generate(&env);
        assert_eq!(client.get_claim_history(&stranger, &0, &10).len(), 0);
    }

    #[test]
    fn test_paginated_reads_on_an_empty_contract() {
        let (env, incentives, _amm, _lp, _reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        assert_eq!(client.get_campaign_count(), 0);
        assert_eq!(client.list_campaigns_paginated(&0, &10).len(), 0);
        assert_eq!(client.get_campaigns_paginated(&0, &10).len(), 0);
        assert_eq!(client.get_active_campaigns_paginated(&0, &10).len(), 0);
        assert_eq!(client.get_campaigns_by_creator(&gov, &0, &10).len(), 0);
        assert_eq!(client.get_claim_history(&provider, &0, &10).len(), 0);
        assert_eq!(client.get_distribution_count(&1), 0);
    }

    // -------------------------------------------------------------------------
    // Issue #826: the `CampaignAccruedRewards` / `CampaignLastAccrualTime` audit
    // trail is exposed via `get_campaign_accrual` and kept in lockstep with the
    // payout accumulator by `claim_rewards`.
    // -------------------------------------------------------------------------

    /// Right after `create_campaign` nothing has accrued and the accrual clock
    /// sits at `start_time`.
    #[test]
    fn test_get_campaign_accrual_initial_state() {
        let (env, incentives, pool, lp, reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        let start = 2_000_u64;
        let id =
            client.create_campaign(&gov, &pool, &lp, &reward, &start, &6_000, &100, &1_000_000);

        assert_eq!(client.get_campaign_accrual(&id), (0_i128, start));
    }

    /// After a claim the audit trail is checkpointed to the claim time, and the
    /// accrued total equals `reward_rate * elapsed` exactly.
    #[test]
    fn test_get_campaign_accrual_after_claim() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        let start = 1_000_u64;
        let end = 11_000_u64;
        let rate = 100_i128;
        let id = client.create_campaign(
            &gov,
            &pool,
            &lp,
            &reward,
            &start,
            &end,
            &rate,
            &((end - start) as i128 * rate),
        );

        // The first claim only initialises the provider snapshot, but it still
        // checkpoints the campaign-level accrual to that moment.
        env.ledger().with_mut(|l| l.timestamp = 3_000);
        assert_eq!(client.claim_rewards(&provider, &id), 0);
        assert_eq!(
            client.get_campaign_accrual(&id),
            (rate * (3_000 - start) as i128, 3_000_u64)
        );

        // The second claim advances the trail by exactly the elapsed seconds.
        env.ledger().with_mut(|l| l.timestamp = 5_500);
        assert!(client.claim_rewards(&provider, &id) > 0);
        assert_eq!(
            client.get_campaign_accrual(&id),
            (rate * (5_500 - start) as i128, 5_500_u64)
        );
    }

    /// Accrual is capped at `end_time`: claiming long after the campaign closed
    /// yields `reward_rate * duration`, not `reward_rate * (now - start)`.
    #[test]
    fn test_get_campaign_accrual_capped_at_end_time() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        let start = 1_000_u64;
        let end = 4_000_u64;
        let rate = 100_i128;
        let funding = (end - start) as i128 * rate;
        let id = client.create_campaign(&gov, &pool, &lp, &reward, &start, &end, &rate, &funding);

        env.ledger().with_mut(|l| l.timestamp = 2_000);
        client.claim_rewards(&provider, &id); // snapshot init

        // Claim far past `end_time`.
        env.ledger().with_mut(|l| l.timestamp = 1_000_000);
        client.claim_rewards(&provider, &id);

        let (accrued, last) = client.get_campaign_accrual(&id);
        assert_eq!(accrued, rate * (end - start) as i128, "capped at end_time");
        assert_eq!(accrued, funding);
        assert_eq!(last, end, "accrual clock never advances past end_time");
    }

    /// The invariant `accrued >= total_distributed` holds across a multi-claim,
    /// multi-provider sequence: the accumulator can only pay out what accrued.
    #[test]
    fn test_get_campaign_accrual_matches_total_distributed_lower_bound() {
        let (env, incentives, amm, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        // A second provider joins the pool, so rewards are split between two
        // snapshots claimed at different times.
        let second = Address::generate(&env);
        let amm_client = AmmPoolClient::new(&env, &amm);
        let info = amm_client.get_info();
        StellarAssetClient::new(&env, &info.token_a).mint(&second, &1_000_000);
        StellarAssetClient::new(&env, &info.token_b).mint(&second, &1_000_000);
        amm_client.add_liquidity(&second, &1_000_000, &1_000_000, &0, &u64::MAX);

        let start = 1_000_u64;
        let end = 21_000_u64;
        let rate = 100_i128;
        let id = client.create_campaign(
            &gov,
            &amm,
            &lp,
            &reward,
            &start,
            &end,
            &rate,
            &((end - start) as i128 * rate),
        );

        for (t, who) in [
            (2_000_u64, provider.clone()),
            (4_000, second.clone()),
            (7_500, provider.clone()),
            (11_000, second.clone()),
            (16_000, provider.clone()),
            (30_000, second.clone()),
        ] {
            env.ledger().with_mut(|l| l.timestamp = t);
            // Some of these are snapshot-initialising claims returning 0; either
            // way the invariant must hold afterwards.
            client.claim_rewards(&who, &id);
            let (accrued, _) = client.get_campaign_accrual(&id);
            let distributed = client.get_campaign(&id).total_distributed;
            assert!(
                accrued >= distributed,
                "accrued must never fall below total_distributed"
            );
        }

        // The campaign paid out something, so this is not a vacuous 0 >= 0.
        assert!(client.get_campaign(&id).total_distributed > 0);
    }

    /// A rate change splits accrual into two segments priced at their own rate.
    /// The trail must be checkpointed at the old rate before the new one lands.
    #[test]
    fn test_get_campaign_accrual_after_rate_change() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        let start = 1_000_u64;
        let end = 11_000_u64;
        let rate_a = 100_i128;
        let rate_b = 250_i128;
        let id = client.create_campaign(
            &gov,
            &pool,
            &lp,
            &reward,
            &start,
            &end,
            &rate_a,
            // Fund for the higher of the two rates so the raise stays covered.
            &((end - start) as i128 * rate_b),
        );

        env.ledger().with_mut(|l| l.timestamp = 2_000);
        client.claim_rewards(&provider, &id); // snapshot init

        // Segment 1: t=1_000..4_000 at rate_a.
        env.ledger().with_mut(|l| l.timestamp = 4_000);
        client.set_campaign_rate(&gov, &id, &rate_b);
        let seg1 = rate_a * (4_000 - start) as i128;
        assert_eq!(client.get_campaign_accrual(&id), (seg1, 4_000_u64));

        // Segment 2: t=4_000..9_000 at rate_b, checkpointed by the claim.
        env.ledger().with_mut(|l| l.timestamp = 9_000);
        client.claim_rewards(&provider, &id);
        let seg2 = rate_b * (9_000 - 4_000) as i128;
        assert_eq!(client.get_campaign_accrual(&id), (seg1 + seg2, 9_000_u64));
    }

    /// The accrual trail stays queryable and correct after the campaign has been
    /// wound down and marked inactive by `recover_leftover_funds`.
    #[test]
    fn test_get_campaign_accrual_after_recover_leftover_funds() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        let start = 1_000_u64;
        let end = 5_000_u64;
        let rate = 100_i128;
        // Overfund so there is a leftover to recover.
        let funding = (end - start) as i128 * rate * 2;
        let id = client.create_campaign(&gov, &pool, &lp, &reward, &start, &end, &rate, &funding);

        env.ledger().with_mut(|l| l.timestamp = 2_000);
        client.claim_rewards(&provider, &id); // snapshot init
        env.ledger().with_mut(|l| l.timestamp = 4_000);
        client.claim_rewards(&provider, &id);

        let treasury = Address::generate(&env);
        env.ledger().with_mut(|l| l.timestamp = 9_000);
        assert!(client.recover_leftover_funds(&gov, &id, &treasury) > 0);

        // Inactive campaign, but the audit trail is intact and capped at end.
        assert!(!client.get_campaign(&id).active);
        let (accrued, last) = client.get_campaign_accrual(&id);
        assert_eq!(accrued, rate * (end - start) as i128);
        assert_eq!(last, end);
        assert!(accrued >= client.get_campaign(&id).total_distributed);
    }

    // -------------------------------------------------------------------------
    // Issue #914: every incentive_campaigns event carries EVENT_SCHEMA_VERSION
    //
    // These publish sites used to call `env.events().publish(...)` directly, so
    // their payloads were not version-stamped and an indexer reading
    // `(version, ...rest)` would have decoded the first real field as the
    // version number. Each test below pins the stamp for one topic.
    // -------------------------------------------------------------------------

    /// Fetch the payload of the most recent event `incentives` published under
    /// `topic`, decoded as a version-stamped `(u32, T)` pair.
    fn last_versioned_event<T>(env: &Env, incentives: &Address, topic: &str) -> (u32, T)
    where
        T: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>,
    {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::IntoVal;

        let wanted: soroban_sdk::Vec<soroban_sdk::Val> =
            (Symbol::new(env, topic),).into_val(env);
        let evt = env
            .events()
            .all()
            .iter()
            .rfind(|e| &e.0 == incentives && e.1 == wanted)
            .unwrap_or_else(|| panic!("no `{topic}` event found"));
        evt.2.into_val(env)
    }

    #[test]
    fn test_governance_proposed_emits_versioned_event() {
        let (env, incentives, _pool, _lp, _reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let new_governance = Address::generate(&env);

        client.propose_governance(&gov, &new_governance);

        let (version, data): (u32, (Address, Address)) =
            last_versioned_event(&env, &incentives, "governance_proposed");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (gov, new_governance));
    }

    #[test]
    fn test_governance_transferred_emits_versioned_event() {
        let (env, incentives, _pool, _lp, _reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let new_governance = Address::generate(&env);

        client.propose_governance(&gov, &new_governance);
        client.accept_governance(&new_governance);

        let (version, data): (u32, (Address, Address)) =
            last_versioned_event(&env, &incentives, "governance_transferred");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (gov, new_governance));
    }

    #[test]
    fn test_campaign_created_emits_versioned_event() {
        let (env, incentives, pool, lp, reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        let id = client.create_campaign(
            &gov, &pool, &lp, &reward, &1_000, &11_000, &100, &1_000_000,
        );

        let (version, data): (u32, (u64, Address, Address, u64, u64, i128)) =
            last_versioned_event(&env, &incentives, "campaign_created");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (id, pool, reward, 1_000, 11_000, 100));
    }

    #[test]
    fn test_rate_updated_emits_versioned_event() {
        let (env, incentives, pool, lp, reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        let id = client.create_campaign(
            &gov, &pool, &lp, &reward, &1_000, &20_000, &100, &2_000_000,
        );
        client.set_campaign_rate(&gov, &id, &200);

        let (version, data): (u32, (u64, i128)) =
            last_versioned_event(&env, &incentives, "rate_updated");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (id, 200));
    }

    #[test]
    fn test_leftover_recovered_emits_versioned_event() {
        let (env, incentives, pool, lp, reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let treasury = Address::generate(&env);

        // Campaign runs 1_000..5_000 at rate 100, funded 1_000_000; no claims.
        let id = client.create_campaign(
            &gov, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000,
        );
        env.ledger().with_mut(|l| l.timestamp = 9_000);
        let recovered = client.recover_leftover_funds(&gov, &id, &treasury);

        let (version, data): (u32, (u64, Address, i128)) =
            last_versioned_event(&env, &incentives, "leftover_recovered");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (id, treasury, recovered));
    }

    #[test]
    fn test_reward_distributed_emits_versioned_event() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        let id = client.create_campaign(
            &gov, &pool, &lp, &reward, &1_000, &11_000, &100, &1_000_000,
        );
        // First claim initialises the snapshot and emits nothing.
        assert_eq!(client.claim_rewards(&provider, &id), 0);
        // Advance and claim again: this distribution is the first audit record.
        env.ledger().with_mut(|l| l.timestamp = 6_000);
        let paid = client.claim_rewards(&provider, &id);
        assert!(paid > 0);

        let (version, data): (u32, (u64, Address, i128, u64)) =
            last_versioned_event(&env, &incentives, "reward_distributed");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (id, provider, paid, 1));
    }

    // #927: typed IncentiveError instead of opaque host traps
    // -------------------------------------------------------------------------

    #[test]
    fn test_pre_init_calls_return_not_initialized() {
        let env = Env::default();
        env.mock_all_auths();
        let addr = env.register_contract(None, IncentiveCampaigns);
        let client = IncentiveCampaignsClient::new(&env, &addr);
        let caller = Address::generate(&env);
        let other = Address::generate(&env);

        assert_eq!(
            client.try_get_governance(),
            Err(Ok(IncentiveError::NotInitialized))
        );
        assert_eq!(
            client.try_propose_governance(&caller, &other),
            Err(Ok(IncentiveError::NotInitialized))
        );
        assert_eq!(
            client.try_create_campaign(&caller, &other, &other, &other, &1_000, &2_000, &1, &1_000),
            Err(Ok(IncentiveError::NotInitialized))
        );
        assert_eq!(
            client.try_set_campaign_rate(&caller, &1, &10),
            Err(Ok(IncentiveError::NotInitialized))
        );
        assert_eq!(
            client.try_recover_leftover_funds(&caller, &1, &other),
            Err(Ok(IncentiveError::NotInitialized))
        );
    }

    #[test]
    fn test_governance_errors_are_typed() {
        let (env, incentives, _, _, _, _, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let stranger = Address::generate(&env);
        let nominee = Address::generate(&env);

        assert_eq!(
            client.try_initialize(&stranger),
            Err(Ok(IncentiveError::AlreadyInitialized))
        );
        assert_eq!(
            client.try_propose_governance(&stranger, &nominee),
            Err(Ok(IncentiveError::Unauthorized))
        );
        assert_eq!(
            client.try_accept_governance(&nominee),
            Err(Ok(IncentiveError::NoPendingGovernance))
        );
        client.propose_governance(&gov, &nominee);
        assert_eq!(
            client.try_accept_governance(&stranger),
            Err(Ok(IncentiveError::NotPendingGovernance))
        );
    }

    #[test]
    fn test_create_campaign_errors_are_typed() {
        let (env, incentives, amm, lp, reward, _, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let stranger = Address::generate(&env);

        assert_eq!(
            client.try_create_campaign(&stranger, &amm, &lp, &reward, &1_000, &2_000, &1, &1_000),
            Err(Ok(IncentiveError::Unauthorized))
        );
        assert_eq!(
            client.try_create_campaign(&gov, &amm, &lp, &reward, &2_000, &2_000, &1, &1_000),
            Err(Ok(IncentiveError::InvalidCampaignWindow))
        );
        assert_eq!(
            client.try_create_campaign(&gov, &amm, &lp, &reward, &1_000, &2_000, &0, &1_000),
            Err(Ok(IncentiveError::InvalidRewardRate))
        );
        assert_eq!(
            client.try_create_campaign(&gov, &amm, &lp, &reward, &1_000, &2_000, &1, &0),
            Err(Ok(IncentiveError::InvalidFundingAmount))
        );
        assert_eq!(
            client.try_create_campaign(&gov, &amm, &lp, &reward, &1_000, &2_000, &1, &999),
            Err(Ok(IncentiveError::InsufficientFunding))
        );
        assert_eq!(
            client.try_create_campaign(&gov, &stranger, &lp, &reward, &1_000, &2_000, &1, &1_000),
            Err(Ok(IncentiveError::LpTokenMismatch))
        );
    }

    #[test]
    fn test_campaign_lookup_and_claim_errors_are_typed() {
        let (env, incentives, amm, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let no_lp = Address::generate(&env);

        assert_eq!(
            client.try_get_campaign(&99),
            Err(Ok(IncentiveError::CampaignNotFound))
        );
        assert_eq!(
            client.try_get_campaign_accrual(&99),
            Err(Ok(IncentiveError::CampaignNotFound))
        );
        assert_eq!(
            client.try_claim_rewards(&provider, &99),
            Err(Ok(IncentiveError::CampaignNotFound))
        );
        assert_eq!(
            client.try_set_campaign_rate(&gov, &99, &10),
            Err(Ok(IncentiveError::CampaignNotFound))
        );
        assert_eq!(
            client.try_get_distribution_record(&99),
            Err(Ok(IncentiveError::RecordNotFound))
        );

        // Campaign that starts in the future.
        let id = client.create_campaign(&gov, &amm, &lp, &reward, &5_000, &6_000, &1, &1_000);
        assert_eq!(
            client.try_set_campaign_rate(&gov, &id, &0),
            Err(Ok(IncentiveError::InvalidRewardRate))
        );
        assert_eq!(
            client.try_claim_rewards(&provider, &id),
            Err(Ok(IncentiveError::CampaignNotStarted))
        );

        env.ledger().set_timestamp(5_500);
        assert_eq!(
            client.try_claim_rewards(&no_lp, &id),
            Err(Ok(IncentiveError::NoLpBalance))
        );

        // Nothing is ever claimed, so after the end every unit is left over.
        env.ledger().set_timestamp(7_000);
        assert_eq!(client.recover_leftover_funds(&gov, &id, &gov), 1_000);
    }

    /// A second `recover_leftover_funds` on the same campaign must not pay out
    /// again. The leftover is computed from `funding_amount - total_distributed`,
    /// which recovery does not change, so without the inactive check the
    /// second call would pull the same amount out of whatever other campaigns
    /// hold in the shared reward token.
    #[test]
    fn test_recover_leftover_funds_cannot_run_twice() {
        let (env, incentives, amm, lp, reward, _, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let reward_client = soroban_sdk::token::Client::new(&env, &reward);
        let treasury = Address::generate(&env);

        let first = client.create_campaign(&gov, &amm, &lp, &reward, &1_000, &2_000, &1, &1_000);
        // A second campaign funded in the same reward token, still running.
        client.create_campaign(&gov, &amm, &lp, &reward, &1_000, &9_000, &1, &8_000);
        assert_eq!(reward_client.balance(&incentives), 9_000);

        env.ledger().set_timestamp(3_000);
        assert_eq!(
            client.recover_leftover_funds(&gov, &first, &treasury),
            1_000
        );
        assert_eq!(
            client.try_recover_leftover_funds(&gov, &first, &treasury),
            Err(Ok(IncentiveError::CampaignInactive))
        );

        // The other campaign's funding is untouched.
        assert_eq!(reward_client.balance(&treasury), 1_000);
        assert_eq!(reward_client.balance(&incentives), 8_000);
    }
}
