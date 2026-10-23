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
//!
//! ## Stake-based campaigns (fix for #1115)
//!
//! The accumulator above was still fed by data this contract cannot observe over
//! time: the LP token's live `total_supply` and each provider's live balance. A
//! provider was only registered by their first `claim_rewards`, so a passive
//! provider's first claim paid 0, and anyone could reprice an idle window by
//! temporarily changing the LP supply right before the next touch.
//!
//! Campaigns created from now on are **stake-based**. Providers `stake` their LP
//! tokens into this contract and `unstake` them again. The contract therefore
//! sees every change to the staked amount as it happens:
//!
//! * the accumulator divides by the campaign's own `total_staked`, which is only
//!   ever changed by `stake` / `unstake`, so each interval is priced at the stake
//!   that existed over it;
//! * `stake`, `unstake` and `set_campaign_rate` all advance the accumulator to
//!   "now" *before* they change anything, so no third party can reprice another
//!   provider's interval;
//! * a position that stays staked for the whole window receives its exact share
//!   on its first claim, however late that claim is made;
//! * seconds with zero stake accrue to nobody and stay in
//!   `funding_amount - total_distributed`, which `recover_leftover_funds` returns.
//!
//! Campaigns that existed before the upgrade have no stake marker and keep
//! running under the old live-balance rules (see `docs/incentive-campaigns.md`).
//!
//! # Pause policy
//!
//! Governance can pause contract activity with `pause` (and resume with `unpause`).
//! While paused, `create_campaign`, `set_campaign_rate` and `claim_rewards` are
//! blocked with `IncentiveError::Paused`. All views, `stake`, `unstake`, and
//! `recover_leftover_funds` (after campaign ends) remain functional.
//! Accrual is unaffected by a pause: rewards accrue strictly by ledger time, so
//! claims after unpause pay exactly what they would have paid without the pause.
//!
//! | Function | While paused | Reason |
//! |---|---|---|
//! | `initialize` | allowed | One-shot setup. |
//! | `propose_governance` | allowed | Governance rotation. |
//! | `accept_governance` | allowed | Governance rotation. |
//! | `get_governance` | allowed | View. |
//! | `get_pending_governance` | allowed | View. |
//! | `create_campaign` | blocked (`Paused`) | Creates a new campaign and takes funding. |
//! | `set_campaign_rate` | blocked (`Paused`) | Modifies campaign reward rate. |
//! | `recover_leftover_funds` | allowed | Post-campaign leftover recovery. |
//! | `stake` | allowed | LP position management. |
//! | `unstake` | allowed | LP position exit; stakers must always be able to withdraw principal. |
//! | `claim_rewards` | blocked (`Paused`) | Reward payout. |
//! | `get_stake` | allowed | View. |
//! | `get_total_staked` | allowed | View. |
//! | `uses_staking` | allowed | View. |
//! | `get_pending_rewards` | allowed | View. |
//! | `get_campaign` | allowed | View. |
//! | `get_campaign_accrual` | allowed | View. |
//! | `list_campaigns` | allowed | View. |
//! | `get_campaigns` | allowed | View. |
//! | `get_distribution_record` | allowed | View. |
//! | `get_active_campaigns` | allowed | View. |
//! | `get_active_campaigns_paged` | allowed | View. |
//! | `get_campaign_count` | allowed | View. |
//! | `list_campaigns_paginated` | allowed | View. |
//! | `get_campaigns_paginated` | allowed | View. |
//! | `get_active_campaigns_paginated` | allowed | View. |
//! | `get_campaigns_by_creator` | allowed | View. |
//! | `is_campaign_active` | allowed | View. |
//! | `get_distribution_count` | allowed | View. |
//! | `list_distribution_records` | allowed | View. |
//! | `get_claim_history` | allowed | View. |
//! | `pause` | allowed | Governance control. |
//! | `unpause` | allowed | Governance control. |
//! | `is_paused` | allowed | View. |
//! | `upgrade` | allowed | Governance control. |
//! | `version` | allowed | View. |

use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype,
    token::Client as TokenClient, Address, BytesN, Env, Symbol, Vec,
};

/// Contract build version returned by `version()`. Bump with each release so
/// integrators (and the deploy verification) can tell which build they talk
/// to, including after an in-place `upgrade`.
pub const CONTRACT_VERSION: u32 = 1;

#[cfg(any(test, feature = "testutils"))]
pub const WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32v1-none/release/incentive_campaigns.wasm"
));

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
    /// The caller has no stake in the campaign.
    NoStake = 20,
    /// `stake` / `unstake` was given a zero or negative amount.
    InvalidStakeAmount = 21,
    /// `unstake` asked for more than the caller has staked.
    InsufficientStake = 22,
    /// `stake` was called at or after the campaign's `end_time`.
    CampaignEnded = 23,
    /// A stake operation was attempted on a campaign created before the
    /// stake-based model; such campaigns keep the live-balance rules.
    LegacyCampaign = 24,
    /// `claim_grace_secs` is outside `[MIN_CLAIM_GRACE_SECS, MAX_CLAIM_GRACE_SECS]`.
    InvalidClaimGrace = 25,
    /// `recover_leftover_funds` was called before `end_time + claim_grace_secs`
    /// had passed, so providers may still be claiming.
    ClaimGraceActive = 26,
    /// `migrate_indexes` was given a `kind` other than 0, 1 or 2.
    InvalidIndexKind = 27,
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
    /// Per campaign: marker present for stake-based campaigns (#1115). Campaigns
    /// created before the upgrade have no marker and use the legacy accounting.
    StakeBased(u64),
    /// Per campaign: LP tokens currently staked, i.e. the accumulator divisor.
    CampaignTotalStaked(u64),
    /// Per (campaign_id, provider): the provider's stake position.
    StakePosition(u64, Address),
    // ---- Appended in #1116 / #1117. Never reorder or insert above this line. ----
    /// Per campaign: seconds after `end_time` during which recovery is blocked.
    /// Absent for campaigns created before #1117, which read back as 0.
    ClaimGraceSecs(u64),
    /// Per campaign: the address that created it (recorded for new campaigns).
    CampaignCreator(u64),
    /// Indexed layout, per campaign: number of distribution records.
    DistCampaignCount(u64),
    /// Indexed layout, per (campaign, n): the n-th distribution record id.
    DistCampaignItem(u64, u32),
    /// Indexed layout, per provider: number of distribution records.
    DistProviderCount(Address),
    /// Indexed layout, per (provider, n): the n-th distribution record id.
    DistProviderItem(Address, u32),
    /// Indexed layout, per creator: number of campaigns created.
    CreatorCount(Address),
    /// Indexed layout, per (creator, n): the n-th campaign id.
    CreatorItem(Address, u32),
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

/// Shortest allowed claim grace period: one hour.
pub const MIN_CLAIM_GRACE_SECS: u64 = 3_600;
/// Longest allowed claim grace period: 90 days.
pub const MAX_CLAIM_GRACE_SECS: u64 = 7_776_000;
/// Grace period applied by `create_campaign`, which takes no explicit value: 7 days.
pub const DEFAULT_CLAIM_GRACE_SECS: u64 = 604_800;

/// `migrate_indexes` kinds.
pub const INDEX_KIND_BY_CAMPAIGN: u32 = 0;
pub const INDEX_KIND_BY_PROVIDER: u32 = 1;
pub const INDEX_KIND_BY_CREATOR: u32 = 2;

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

/// A provider's position in a stake-based campaign (#1115).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StakePosition {
    /// LP tokens this provider currently has staked.
    pub amount: i128,
    /// Value of `acc_reward_per_share` when `pending` was last settled.
    pub acc_at_snapshot: i128,
    /// Rewards already earned but not yet paid. Filled whenever the stake
    /// changes (so a change never forfeits or re-prices earlier seconds) and
    /// cleared by `claim_rewards`.
    pub pending: i128,
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

/// Stake-based counterpart of [`advance_accumulator`] (#1115).
///
/// Always moves `last_update_time` forward, even when nothing is staked: the
/// seconds with zero stake are consumed without crediting anyone, so they can
/// never be priced later at a different stake. `total_staked` must be the stake
/// that was in place over the whole interval, which holds because every change
/// to it first calls this function.
fn advance_staked_accumulator(
    mut campaign: Campaign,
    up_to_time: u64,
    total_staked: i128,
) -> Campaign {
    if up_to_time <= campaign.last_update_time {
        return campaign;
    }
    if total_staked > 0 {
        let elapsed = (up_to_time - campaign.last_update_time) as i128;
        campaign.acc_reward_per_share += campaign.reward_rate * elapsed * PRECISION / total_staked;
    }
    campaign.last_update_time = up_to_time;
    campaign
}

// ---------------------------------------------------------------------------
// Append-only indexes (#1116)
//
// Each index is a `Count` entry plus one entry per element, so appending is
// O(1) however long the index already is. The previous layout stored one
// `Vec<u64>` per owner and rewrote the whole vector on every append, which made
// the cost of a claim grow with the number of prior claims.
//
// An owner whose index still lives in the legacy `Vec` (no `Count` entry yet)
// is read from that vector until it is migrated, either by `migrate_indexes` or
// by the first append, so every view returns the same answer before and after.
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[allow(clippy::enum_variant_names)]
enum Index {
    ByCampaign(u64),
    ByProvider(Address),
    ByCreator(Address),
}

impl Index {
    fn count_key(&self) -> DataKey {
        match self {
            Index::ByCampaign(c) => DataKey::DistCampaignCount(*c),
            Index::ByProvider(a) => DataKey::DistProviderCount(a.clone()),
            Index::ByCreator(a) => DataKey::CreatorCount(a.clone()),
        }
    }

    fn item_key(&self, n: u32) -> DataKey {
        match self {
            Index::ByCampaign(c) => DataKey::DistCampaignItem(*c, n),
            Index::ByProvider(a) => DataKey::DistProviderItem(a.clone(), n),
            Index::ByCreator(a) => DataKey::CreatorItem(a.clone(), n),
        }
    }

    fn legacy_key(&self) -> DataKey {
        match self {
            Index::ByCampaign(c) => DataKey::DistributionsByCampaign(*c),
            Index::ByProvider(a) => DataKey::DistributionsByProvider(a.clone()),
            Index::ByCreator(a) => DataKey::CampaignsByCreator(a.clone()),
        }
    }
}

fn legacy_vec(env: &Env, index: &Index) -> Option<Vec<u64>> {
    let key = index.legacy_key();
    let v = env.storage().persistent().get::<DataKey, Vec<u64>>(&key)?;
    extend_persistent_ttl(env, &key);
    Some(v)
}

/// Number of elements in `index`, from whichever layout currently holds it.
fn index_len(env: &Env, index: &Index) -> u32 {
    let ck = index.count_key();
    if let Some(n) = env.storage().persistent().get::<DataKey, u32>(&ck) {
        extend_persistent_ttl(env, &ck);
        return n;
    }
    legacy_vec(env, index).map(|v| v.len()).unwrap_or(0)
}

/// Elements `[start, end)` of `index`, in append order.
fn index_slice(env: &Env, index: &Index, start: u32, end: u32) -> Vec<u64> {
    let mut out = Vec::new(env);
    let ck = index.count_key();
    if env.storage().persistent().has(&ck) {
        extend_persistent_ttl(env, &ck);
        for n in start..end {
            let key = index.item_key(n);
            if let Some(id) = env.storage().persistent().get::<DataKey, u64>(&key) {
                extend_persistent_ttl(env, &key);
                out.push_back(id);
            }
        }
    } else if let Some(v) = legacy_vec(env, index) {
        for n in start..end {
            if let Some(id) = v.get(n) {
                out.push_back(id);
            }
        }
    }
    out
}

/// Move `index` from the legacy vector into the indexed layout. Returns whether
/// anything was moved; an already-migrated or never-written owner is a no-op.
fn migrate_index(env: &Env, index: &Index) -> bool {
    let ck = index.count_key();
    if env.storage().persistent().has(&ck) {
        return false;
    }
    let lk = index.legacy_key();
    let Some(v) = env.storage().persistent().get::<DataKey, Vec<u64>>(&lk) else {
        return false;
    };
    for n in 0..v.len() {
        let key = index.item_key(n);
        env.storage().persistent().set(&key, &v.get(n).unwrap());
        extend_persistent_ttl(env, &key);
    }
    env.storage().persistent().set(&ck, &v.len());
    extend_persistent_ttl(env, &ck);
    env.storage().persistent().remove(&lk);
    true
}

/// Append `id` to `index` in O(1).
fn index_push(env: &Env, index: &Index, id: u64) {
    let ck = index.count_key();
    let mut count: Option<u32> = env.storage().persistent().get(&ck);
    if count.is_none() && migrate_index(env, index) {
        count = env.storage().persistent().get(&ck);
    }
    let n = count.unwrap_or(0);
    let key = index.item_key(n);
    env.storage().persistent().set(&key, &id);
    extend_persistent_ttl(env, &key);
    env.storage().persistent().set(&ck, &(n + 1));
    extend_persistent_ttl(env, &ck);
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct IncentiveCampaigns;

#[contractimpl]
impl IncentiveCampaigns {
    /// Set the governance address. Callable once.
    ///
    /// Deliberately does **not** `require_auth` on `governance`: the address is
    /// typically a governance contract that cannot sign a deploy transaction.
    /// Initialization is verified instead of authenticated: the `initialized`
    /// event and `get_governance` report what was stored, and the deploy script
    /// reads it back and aborts on a mismatch, which exposes a front-run.
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

        // The payload is exactly what was just written to instance storage, so
        // a deploy script can compare it (or the getters) with what it intended.
        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "initialized"),),
            (governance, 1u64, 1u64)
        );
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

    /// Halt contract activity (campaign creation, rate updates, reward claims). Governance only.
    pub fn pause(env: Env, caller: Address) -> Result<(), IncentiveError> {
        extend_instance_ttl(&env);
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        env.storage().instance().set(&DataKey::Paused, &true);
        soroban_amm_sdk::emit_versioned_event!(&env, (Symbol::new(&env, "paused"),), (caller,));
        Ok(())
    }

    /// Resume contract activity. Governance only.
    pub fn unpause(env: Env, caller: Address) -> Result<(), IncentiveError> {
        extend_instance_ttl(&env);
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        env.storage().instance().set(&DataKey::Paused, &false);
        soroban_amm_sdk::emit_versioned_event!(&env, (Symbol::new(&env, "unpaused"),), (caller,));
        Ok(())
    }

    /// Return whether the contract is currently paused.
    pub fn is_paused(env: Env) -> bool {
        extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Replace this contract's WASM with a new version. Governance only.
    pub fn upgrade(
        env: Env,
        caller: Address,
        new_wasm_hash: BytesN<32>,
    ) -> Result<(), IncentiveError> {
        extend_instance_ttl(&env);
        caller.require_auth();
        Self::require_governance(&env, &caller)?;
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "upgraded"),),
            (new_wasm_hash,)
        );
        Ok(())
    }

    /// Return the build version of this contract (`CONTRACT_VERSION`).
    pub fn version(_env: Env) -> u32 {
        CONTRACT_VERSION
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

    /// Create a time-based incentive campaign with the default claim grace
    /// period ([`DEFAULT_CLAIM_GRACE_SECS`]). Governance only.
    ///
    /// See [`IncentiveCampaigns::create_campaign_with_grace`].
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
        Self::create_campaign_with_grace(
            env,
            caller,
            pool,
            lp_token,
            reward_token,
            start_time,
            end_time,
            reward_rate,
            funding_amount,
            DEFAULT_CLAIM_GRACE_SECS,
        )
    }

    /// Create a time-based incentive campaign. Governance only.
    ///
    /// `claim_grace_secs` is how long after `end_time` providers can keep
    /// claiming before `recover_leftover_funds` may sweep the leftover. It must
    /// lie in `[MIN_CLAIM_GRACE_SECS, MAX_CLAIM_GRACE_SECS]`.
    ///
    /// The contract measures what it actually receives (balance after minus
    /// balance before the transfer), requires it to cover
    /// `reward_rate * (end_time - start_time)` and stores it as
    /// `funding_amount`. A fee-on-transfer reward token therefore cannot leave
    /// the campaign under-funded relative to its payout promise.
    #[allow(clippy::too_many_arguments)]
    pub fn create_campaign_with_grace(
        env: Env,
        caller: Address,
        pool: Address,
        lp_token: Address,
        reward_token: Address,
        start_time: u64,
        end_time: u64,
        reward_rate: i128,
        funding_amount: i128,
        claim_grace_secs: u64,
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
        if !(MIN_CLAIM_GRACE_SECS..=MAX_CLAIM_GRACE_SECS).contains(&claim_grace_secs) {
            return Err(IncentiveError::InvalidClaimGrace);
        }
        let duration = (end_time - start_time) as i128;
        let max_payout = reward_rate
            .checked_mul(duration)
            .ok_or(IncentiveError::InvalidRewardRate)?;
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

        // Pull the funding first and measure what arrived. A failure below
        // reverts the whole invocation, transfer included.
        let contract = env.current_contract_address();
        let token = TokenClient::new(&env, &reward_token);
        let balance_before = token.balance(&contract);
        token.transfer(&caller, &contract, &funding_amount);
        let received = token
            .balance(&contract)
            .checked_sub(balance_before)
            .ok_or(IncentiveError::InsufficientFunding)?;
        if received < max_payout {
            return Err(IncentiveError::InsufficientFunding);
        }

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
            funding_amount: received,
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

        let grace_key = DataKey::ClaimGraceSecs(id);
        env.storage()
            .persistent()
            .set(&grace_key, &claim_grace_secs);
        extend_persistent_ttl(&env, &grace_key);
        let creator_key = DataKey::CampaignCreator(id);
        env.storage().persistent().set(&creator_key, &caller);
        extend_persistent_ttl(&env, &creator_key);
        // New campaigns are stake-based (#1115).
        let stake_marker_key = DataKey::StakeBased(id);
        env.storage().persistent().set(&stake_marker_key, &true);
        extend_persistent_ttl(&env, &stake_marker_key);

        env.storage()
            .instance()
            .set(&DataKey::NextCampaignId, &(id + 1));

        let index = id - 1;
        let index_key = DataKey::CampaignIdByIndex(index);
        env.storage().persistent().set(&index_key, &id);
        extend_persistent_ttl(&env, &index_key);

        index_push(&env, &Index::ByCreator(caller.clone()), id);

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
        Self::ensure_not_paused(&env)?;
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
        campaign = if Self::is_stake_based(&env, campaign_id) {
            let total_staked = Self::read_total_staked(&env, campaign_id);
            advance_staked_accumulator(campaign, accrual_until, total_staked)
        } else {
            let total_supply = LpTokenClient::new(&env, &campaign.lp_token).total_supply();
            advance_accumulator(campaign, accrual_until, total_supply)
        };

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
    /// Recovery is rejected with `ClaimGraceActive` until the ledger time is
    /// strictly past `end_time + claim_grace_secs`, so providers always get that
    /// window to claim first. Campaigns created before the grace period existed
    /// have no stored value and read back as 0, so they behave as they did.
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
        let grace = Self::claim_grace(&env, campaign_id);
        if now <= campaign.end_time.saturating_add(grace) {
            return Err(IncentiveError::ClaimGraceActive);
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

    /// Stake LP tokens into a stake-based campaign (#1115).
    ///
    /// The LP tokens move from `provider` into this contract and count towards
    /// the campaign's `total_staked` from this ledger on. Staking before
    /// `start_time` is allowed (rewards only start flowing at `start_time`);
    /// staking at or after `end_time` is rejected because nothing could accrue.
    ///
    /// Everything accrued so far is settled against the *old* stake first, so
    /// adding stake neither re-prices earlier seconds nor shares them with the
    /// new stake.
    pub fn stake(
        env: Env,
        provider: Address,
        campaign_id: u64,
        amount: i128,
    ) -> Result<(), IncentiveError> {
        extend_instance_ttl(&env);
        provider.require_auth();
        if amount <= 0 {
            return Err(IncentiveError::InvalidStakeAmount);
        }

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
        if !Self::is_stake_based(&env, campaign_id) {
            return Err(IncentiveError::LegacyCampaign);
        }
        let now = env.ledger().timestamp();
        if now >= campaign.end_time {
            return Err(IncentiveError::CampaignEnded);
        }

        let accrual_until = Self::campaign_accrual_time(&campaign, now);
        let total_staked = Self::read_total_staked(&env, campaign_id);
        campaign = advance_staked_accumulator(campaign, accrual_until, total_staked);
        Self::checkpoint_campaign_rewards(&env, campaign_id, &campaign, now);

        let position_key = DataKey::StakePosition(campaign_id, provider.clone());
        let mut position: StakePosition =
            env.storage()
                .persistent()
                .get(&position_key)
                .unwrap_or(StakePosition {
                    amount: 0,
                    acc_at_snapshot: campaign.acc_reward_per_share,
                    pending: 0,
                });
        position.pending = Self::position_pending(&position, campaign.acc_reward_per_share);
        position.acc_at_snapshot = campaign.acc_reward_per_share;
        position.amount += amount;
        env.storage().persistent().set(&position_key, &position);
        extend_persistent_ttl(&env, &position_key);

        let new_total = total_staked + amount;
        Self::write_total_staked(&env, campaign_id, new_total);
        env.storage().persistent().set(&campaign_key, &campaign);
        extend_persistent_ttl(&env, &campaign_key);

        let contract = env.current_contract_address();
        TokenClient::new(&env, &campaign.lp_token).transfer(&provider, &contract, &amount);

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "staked"),),
            (campaign_id, provider, amount, new_total)
        );
        Ok(())
    }

    /// Withdraw staked LP tokens from a stake-based campaign (#1115).
    ///
    /// Always available, including after the campaign ended or was wound down by
    /// `recover_leftover_funds`: principal is never held hostage. Rewards earned
    /// up to now are kept on the position and stay claimable through
    /// `claim_rewards` while the campaign is active.
    pub fn unstake(
        env: Env,
        provider: Address,
        campaign_id: u64,
        amount: i128,
    ) -> Result<(), IncentiveError> {
        extend_instance_ttl(&env);
        provider.require_auth();
        if amount <= 0 {
            return Err(IncentiveError::InvalidStakeAmount);
        }

        let campaign_key = DataKey::Campaign(campaign_id);
        let mut campaign: Campaign = env
            .storage()
            .persistent()
            .get(&campaign_key)
            .ok_or(IncentiveError::CampaignNotFound)?;
        extend_persistent_ttl(&env, &campaign_key);
        if !Self::is_stake_based(&env, campaign_id) {
            return Err(IncentiveError::LegacyCampaign);
        }

        let position_key = DataKey::StakePosition(campaign_id, provider.clone());
        let mut position: StakePosition = env
            .storage()
            .persistent()
            .get(&position_key)
            .ok_or(IncentiveError::NoStake)?;
        if amount > position.amount {
            return Err(IncentiveError::InsufficientStake);
        }

        let now = env.ledger().timestamp();
        let accrual_until = Self::campaign_accrual_time(&campaign, now);
        let total_staked = Self::read_total_staked(&env, campaign_id);
        campaign = advance_staked_accumulator(campaign, accrual_until, total_staked);
        Self::checkpoint_campaign_rewards(&env, campaign_id, &campaign, now);

        position.pending = Self::position_pending(&position, campaign.acc_reward_per_share);
        position.acc_at_snapshot = campaign.acc_reward_per_share;
        position.amount -= amount;
        if position.amount == 0 && position.pending == 0 {
            env.storage().persistent().remove(&position_key);
        } else {
            env.storage().persistent().set(&position_key, &position);
            extend_persistent_ttl(&env, &position_key);
        }

        let new_total = total_staked - amount;
        Self::write_total_staked(&env, campaign_id, new_total);
        env.storage().persistent().set(&campaign_key, &campaign);
        extend_persistent_ttl(&env, &campaign_key);

        let contract = env.current_contract_address();
        TokenClient::new(&env, &campaign.lp_token).transfer(&contract, &provider, &amount);

        soroban_amm_sdk::emit_versioned_event!(
            &env,
            (Symbol::new(&env, "unstaked"),),
            (campaign_id, provider, amount, new_total)
        );
        Ok(())
    }

    /// Pay a provider the rewards they have earned in a campaign.
    ///
    /// ## Stake-based campaigns (#1115)
    ///
    /// The payout is `position.pending + position.amount * (acc_now -
    /// position.acc_at_snapshot) / PRECISION`, where `acc_now` is the campaign
    /// accumulator advanced to `min(now, end_time)` at the stake that existed over
    /// each interval. The first claim of a position that was staked for the whole
    /// window therefore pays its exact share, however late it is made, and no
    /// other address can change that amount by touching the campaign.
    ///
    /// Claiming works after `end_time` for whatever accrued up to `end_time`.
    /// A caller with no stake gets `NoStake`; a caller with nothing new to
    /// collect gets `NoPendingRewards`.
    ///
    /// ## Campaigns created before the upgrade
    ///
    /// Campaigns without a stake marker keep the original live-balance rules: the
    /// first call registers the provider and pays 0, later calls pay from the
    /// provider's live LP balance. See `docs/incentive-campaigns.md`.
    pub fn claim_rewards(
        env: Env,
        provider: Address,
        campaign_id: u64,
    ) -> Result<i128, IncentiveError> {
        Self::ensure_not_paused(&env)?;
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

        if !Self::is_stake_based(&env, campaign_id) {
            return Self::claim_rewards_legacy(&env, &provider, campaign_id, campaign, now);
        }

        // Cap accrual at end_time so providers can still claim earned rewards
        // after the window closes without accruing phantom future rewards.
        let claim_time = now.min(campaign.end_time);
        let total_staked = Self::read_total_staked(&env, campaign_id);
        campaign = advance_staked_accumulator(campaign, claim_time, total_staked);

        // Keep the parallel audit trail (`CampaignAccruedRewards` /
        // `CampaignLastAccrualTime`) in lockstep with the payout accumulator.
        Self::checkpoint_campaign_rewards(&env, campaign_id, &campaign, claim_time);

        let position_key = DataKey::StakePosition(campaign_id, provider.clone());
        let mut position: StakePosition = env
            .storage()
            .persistent()
            .get(&position_key)
            .ok_or(IncentiveError::NoStake)?;
        extend_persistent_ttl(&env, &position_key);

        let mut pending = Self::position_pending(&position, campaign.acc_reward_per_share);
        // Never pay out more than the campaign was funded with, whatever the
        // rate history looks like.
        let unpaid_funding = campaign.funding_amount - campaign.total_distributed;
        if pending > unpaid_funding {
            pending = unpaid_funding;
        }
        if pending <= 0 {
            return Err(IncentiveError::NoPendingRewards);
        }

        position.pending = 0;
        position.acc_at_snapshot = campaign.acc_reward_per_share;
        env.storage().persistent().set(&position_key, &position);
        extend_persistent_ttl(&env, &position_key);

        campaign.total_distributed += pending;
        env.storage().persistent().set(&campaign_key, &campaign);
        extend_persistent_ttl(&env, &campaign_key);

        let contract = env.current_contract_address();
        TokenClient::new(&env, &campaign.reward_token).transfer(&contract, &provider, &pending);

        Self::record_distribution(
            &env,
            campaign_id,
            &provider,
            &campaign.reward_token,
            pending,
            now,
        )?;
        Ok(pending)
    }

    /// LP tokens `provider` currently has staked in `campaign_id` (0 if none).
    pub fn get_stake(env: Env, campaign_id: u64, provider: Address) -> i128 {
        extend_instance_ttl(&env);
        let key = DataKey::StakePosition(campaign_id, provider);
        let position: Option<StakePosition> = env.storage().persistent().get(&key);
        match position {
            Some(position) => {
                extend_persistent_ttl(&env, &key);
                position.amount
            }
            None => 0,
        }
    }

    /// Total LP tokens staked in `campaign_id`: the accumulator's divisor.
    pub fn get_total_staked(env: Env, campaign_id: u64) -> i128 {
        extend_instance_ttl(&env);
        Self::read_total_staked(&env, campaign_id)
    }

    /// Whether `campaign_id` uses the stake-based model (`true`) or the legacy
    /// live-balance rules (`false`, including for ids that do not exist).
    pub fn uses_staking(env: Env, campaign_id: u64) -> bool {
        extend_instance_ttl(&env);
        Self::is_stake_based(&env, campaign_id)
    }

    /// What `claim_rewards` would pay `provider` right now, without sending a
    /// transaction. Returns 0 for a provider with no stake.
    pub fn get_pending_rewards(
        env: Env,
        campaign_id: u64,
        provider: Address,
    ) -> Result<i128, IncentiveError> {
        extend_instance_ttl(&env);
        let campaign_key = DataKey::Campaign(campaign_id);
        let campaign: Campaign = env
            .storage()
            .persistent()
            .get(&campaign_key)
            .ok_or(IncentiveError::CampaignNotFound)?;
        extend_persistent_ttl(&env, &campaign_key);
        if !Self::is_stake_based(&env, campaign_id) {
            return Err(IncentiveError::LegacyCampaign);
        }

        let position_key = DataKey::StakePosition(campaign_id, provider);
        let position: Option<StakePosition> = env.storage().persistent().get(&position_key);
        let position = match position {
            Some(position) => position,
            None => return Ok(0),
        };
        extend_persistent_ttl(&env, &position_key);

        let now = env.ledger().timestamp();
        let accrual_until = Self::campaign_accrual_time(&campaign, now);
        let total_staked = Self::read_total_staked(&env, campaign_id);
        let unpaid_funding = campaign.funding_amount - campaign.total_distributed;
        let advanced = advance_staked_accumulator(campaign, accrual_until, total_staked);
        let pending = Self::position_pending(&position, advanced.acc_reward_per_share);
        Ok(pending.min(unpaid_funding).max(0))
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
        Self::page_index(&env, &Index::ByCreator(creator), offset, limit)
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
        index_len(&env, &Index::ByCampaign(campaign_id))
    }

    /// Page through a campaign's distribution records, oldest claim first.
    pub fn list_distribution_records(
        env: Env,
        campaign_id: u64,
        offset: u32,
        limit: u32,
    ) -> Vec<DistributionRecord> {
        extend_instance_ttl(&env);
        let ids = Self::page_index(&env, &Index::ByCampaign(campaign_id), offset, limit);
        Self::resolve_records(&env, &ids)
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
        let ids = Self::page_index(&env, &Index::ByProvider(provider), offset, limit);
        Self::resolve_records(&env, &ids)
    }

    /// Seconds after `end_time` during which `recover_leftover_funds` is blocked
    /// for `campaign_id`. Campaigns created before #1117 report 0.
    pub fn get_claim_grace(env: Env, campaign_id: u64) -> Result<u64, IncentiveError> {
        extend_instance_ttl(&env);
        if !env
            .storage()
            .persistent()
            .has(&DataKey::Campaign(campaign_id))
        {
            return Err(IncentiveError::CampaignNotFound);
        }
        Ok(Self::claim_grace(&env, campaign_id))
    }

    /// Move legacy `Vec`-based indexes into the indexed layout. Permissionless.
    ///
    /// `kind` selects the index: `0` distributions by campaign, `1` by provider,
    /// `2` campaigns by creator. `offset`/`limit` page over the owners to visit
    /// (campaign index for kinds 0 and 2, distribution record id for kind 1) and
    /// `limit` is clamped to [`MAX_PAGE`]. Returns how many owners were moved.
    ///
    /// Safe to call repeatedly and in any order: an owner that is already
    /// migrated, or never had a legacy vector, is skipped. Until an owner is
    /// migrated its views keep reading the legacy vector, and its next append
    /// migrates it. Legacy creator vectors are located through the recorded
    /// creator of each campaign, falling back to the current governance for
    /// campaigns created before creators were recorded; any other legacy creator
    /// is migrated lazily on its next append and is served correctly meanwhile.
    pub fn migrate_indexes(
        env: Env,
        kind: u32,
        offset: u32,
        limit: u32,
    ) -> Result<u32, IncentiveError> {
        extend_instance_ttl(&env);
        if kind > INDEX_KIND_BY_CREATOR {
            return Err(IncentiveError::InvalidIndexKind);
        }
        let total = if kind == INDEX_KIND_BY_PROVIDER {
            env.storage()
                .instance()
                .get::<DataKey, u64>(&DataKey::NextDistributionId)
                .unwrap_or(1)
                .saturating_sub(1)
        } else {
            Self::campaign_count(&env)
        };
        let Some((start, end)) = Self::page_bounds(offset, limit, total) else {
            return Ok(0);
        };
        let mut moved: u32 = 0;
        for p in start..end {
            let index = match kind {
                INDEX_KIND_BY_CAMPAIGN => Self::campaign_id_at(&env, p).map(Index::ByCampaign),
                INDEX_KIND_BY_PROVIDER => {
                    let key = DataKey::DistributionRecord(p + 1);
                    env.storage()
                        .persistent()
                        .get::<DataKey, DistributionRecord>(&key)
                        .map(|r| Index::ByProvider(r.provider))
                }
                _ => Self::campaign_id_at(&env, p).and_then(|id| {
                    env.storage()
                        .persistent()
                        .get::<DataKey, Address>(&DataKey::CampaignCreator(id))
                        .or_else(|| Self::read_governance(&env).ok())
                        .map(Index::ByCreator)
                }),
            };
            if let Some(index) = index {
                if migrate_index(&env, &index) {
                    moved += 1;
                }
            }
        }
        Ok(moved)
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

    fn claim_grace(env: &Env, campaign_id: u64) -> u64 {
        let key = DataKey::ClaimGraceSecs(campaign_id);
        match env.storage().persistent().get::<DataKey, u64>(&key) {
            Some(g) => {
                extend_persistent_ttl(env, &key);
                g
            }
            None => 0,
        }
    }

    /// Page through an index with the shared pagination rules.
    fn page_index(env: &Env, index: &Index, offset: u32, limit: u32) -> Vec<u64> {
        let count = index_len(env, index) as u64;
        let Some((start, end)) = Self::page_bounds(offset, limit, count) else {
            return Vec::new(env);
        };
        index_slice(env, index, start as u32, end as u32)
    }

    /// Resolve a page of record ids to their stored records.
    fn resolve_records(env: &Env, ids: &Vec<u64>) -> Vec<DistributionRecord> {
        let mut page: Vec<DistributionRecord> = Vec::new(env);
        for i in 0..ids.len() {
            let key = DataKey::DistributionRecord(ids.get(i).unwrap());
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

    fn ensure_not_paused(env: &Env) -> Result<(), IncentiveError> {
        let paused: bool = env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false);
        if paused {
            return Err(IncentiveError::Paused);
        }
        Ok(())
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

    fn is_stake_based(env: &Env, campaign_id: u64) -> bool {
        env.storage()
            .persistent()
            .has(&DataKey::StakeBased(campaign_id))
    }

    fn read_total_staked(env: &Env, campaign_id: u64) -> i128 {
        let key = DataKey::CampaignTotalStaked(campaign_id);
        let total: Option<i128> = env.storage().persistent().get(&key);
        match total {
            Some(total) => {
                extend_persistent_ttl(env, &key);
                total
            }
            None => 0,
        }
    }

    fn write_total_staked(env: &Env, campaign_id: u64, total: i128) {
        let key = DataKey::CampaignTotalStaked(campaign_id);
        env.storage().persistent().set(&key, &total);
        extend_persistent_ttl(env, &key);
    }

    /// Rewards owed to `position` when the accumulator stands at `acc_now`.
    fn position_pending(position: &StakePosition, acc_now: i128) -> i128 {
        position.pending + position.amount * (acc_now - position.acc_at_snapshot) / PRECISION
    }

    /// Write the audit record for one payout, index it per campaign and per
    /// provider, and emit `reward_distributed`. Returns the new record id.
    fn record_distribution(
        env: &Env,
        campaign_id: u64,
        provider: &Address,
        reward_token: &Address,
        amount: i128,
        now: u64,
    ) -> Result<u64, IncentiveError> {
        let dist_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextDistributionId)
            .ok_or(IncentiveError::NotInitialized)?;
        let record = DistributionRecord {
            id: dist_id,
            campaign_id,
            provider: provider.clone(),
            reward_token: reward_token.clone(),
            amount,
            timestamp: now,
        };
        let dist_key = DataKey::DistributionRecord(dist_id);
        env.storage().persistent().set(&dist_key, &record);
        extend_persistent_ttl(env, &dist_key);
        env.storage()
            .instance()
            .set(&DataKey::NextDistributionId, &(dist_id + 1));

        // Index the record both ways so it can be found without already knowing
        // its id. Both indexes are append-only and O(1) per claim.
        index_push(env, &Index::ByCampaign(campaign_id), dist_id);
        index_push(env, &Index::ByProvider(provider.clone()), dist_id);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(env, "reward_distributed"),),
            (campaign_id, provider.clone(), amount, dist_id)
        );
        Ok(dist_id)
    }

    /// The original live-balance claim, kept verbatim for campaigns created
    /// before the stake-based model (no `StakeBased` marker). The caller has
    /// already checked that the campaign is active and has started.
    fn claim_rewards_legacy(
        env: &Env,
        provider: &Address,
        campaign_id: u64,
        mut campaign: Campaign,
        now: u64,
    ) -> Result<i128, IncentiveError> {
        let campaign_key = DataKey::Campaign(campaign_id);
        let claim_time = now.min(campaign.end_time);

        let lp_balance = LpTokenClient::new(env, &campaign.lp_token).balance(provider);
        if lp_balance <= 0 {
            return Err(IncentiveError::NoLpBalance);
        }

        let total_supply = LpTokenClient::new(env, &campaign.lp_token).total_supply();
        if total_supply <= 0 {
            return Err(IncentiveError::NoLpSupply);
        }

        campaign = advance_accumulator(campaign, claim_time, total_supply);
        Self::checkpoint_campaign_rewards(env, campaign_id, &campaign, claim_time);

        let snapshot_key = DataKey::ProviderSnapshot(campaign_id, provider.clone());
        let snapshot: Option<ProviderSnapshot> = env.storage().persistent().get(&snapshot_key);
        if snapshot.is_some() {
            extend_persistent_ttl(env, &snapshot_key);
        }

        let pending = match snapshot {
            None => {
                // First contact: register the provider and pay nothing.
                let init = ProviderSnapshot {
                    acc_at_snapshot: campaign.acc_reward_per_share,
                };
                env.storage().persistent().set(&snapshot_key, &init);
                extend_persistent_ttl(env, &snapshot_key);
                env.storage().persistent().set(&campaign_key, &campaign);
                extend_persistent_ttl(env, &campaign_key);
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

        let contract = env.current_contract_address();
        TokenClient::new(env, &campaign.reward_token).transfer(&contract, provider, &pending);

        let new_snapshot = ProviderSnapshot {
            acc_at_snapshot: campaign.acc_reward_per_share,
        };
        env.storage().persistent().set(&snapshot_key, &new_snapshot);
        extend_persistent_ttl(env, &snapshot_key);

        campaign.total_distributed += pending;
        env.storage().persistent().set(&campaign_key, &campaign);
        extend_persistent_ttl(env, &campaign_key);

        Self::record_distribution(
            env,
            campaign_id,
            provider,
            &campaign.reward_token,
            pending,
            now,
        )?;
        Ok(pending)
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
    extern crate std;

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

    /// Turn a campaign into a pre-upgrade one by dropping its stake marker. The
    /// tests that exercise the original live-balance accounting (first claim
    /// registers the provider and pays 0) run against such campaigns, which keeps
    /// the legacy path covered now that `create_campaign` makes stake-based ones.
    fn make_legacy(env: &Env, incentives: &Address, campaign_id: u64) {
        env.as_contract(incentives, || {
            env.storage()
                .persistent()
                .remove(&DataKey::StakeBased(campaign_id));
        });
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
        make_legacy(&env, &incentives, id);

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
        make_legacy(&env, &incentives, id);

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
        make_legacy(&env, &incentives, id1);
        let id2 = client.create_campaign(
            &gov_addr, &pool, &lp, &reward, &1_000, &20_000, &50, &1_000_000,
        );
        make_legacy(&env, &incentives, id2);
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
        make_legacy(&env, &incentives, id);

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
        make_legacy(&env, &incentives, id2);

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
        make_legacy(&env, &incentives, id);

        // Establish provider snapshot at t=1_000 (returns 0, nothing accrued yet).
        assert_eq!(client.claim_rewards(&provider, &id), 0);

        // Partial claim at t=2_000 (1_000 s elapsed since snapshot).
        env.ledger().with_mut(|l| l.timestamp = 2_000);
        let claimed = client.claim_rewards(&provider, &id);
        // 100 * 1_000 * 1e12 / 1_000_000 = 1e8; 999_000 * 1e8 / 1e12 = 99_900
        assert_eq!(claimed, 99_900);

        // Advance past end_time and recover leftover.
        // The default claim grace is 7 days, so recovery opens at 609_801.
        env.ledger().with_mut(|l| l.timestamp = 700_000);
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
        make_legacy(&env, &incentives, id2);
        env.ledger().with_mut(|l| l.timestamp = 710_000);
        let full_recovery = client.recover_leftover_funds(&gov_addr, &id2, &treasury);
        assert_eq!(full_recovery, 1_000_000);

        // ── Recovery before end_time must be rejected ─────────────────────────────
        let id3 = client.create_campaign(
            &gov_addr, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000,
        );
        make_legacy(&env, &incentives, id3);
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
        make_legacy(&env, &incentives, id);

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
        make_legacy(&env, &incentives, id);

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
        make_legacy(&env, &incentives, id);

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
        make_legacy(env, &client.address, id);
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
        make_legacy(&env, &incentives, second);
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
        make_legacy(&env, &incentives, id);

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
        make_legacy(&env, &incentives, id);

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
        make_legacy(&env, &incentives, id);

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
        make_legacy(&env, &incentives, id);

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
        make_legacy(&env, &incentives, id);

        env.ledger().with_mut(|l| l.timestamp = 2_000);
        client.claim_rewards(&provider, &id); // snapshot init
        env.ledger().with_mut(|l| l.timestamp = 4_000);
        client.claim_rewards(&provider, &id);

        let treasury = Address::generate(&env);
        env.ledger().with_mut(|l| l.timestamp = 700_000);
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

        let wanted: soroban_sdk::Vec<soroban_sdk::Val> = (Symbol::new(env, topic),).into_val(env);
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

        let id =
            client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &11_000, &100, &1_000_000);

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

        let id =
            client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &20_000, &100, &2_000_000);
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
        let id =
            client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000);
        env.ledger().with_mut(|l| l.timestamp = 700_000);
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

        let id =
            client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &11_000, &100, &1_000_000);
        make_legacy(&env, &incentives, id);
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
            Err(Ok(IncentiveError::NoStake))
        );

        // Nothing is ever claimed, so after the end every unit is left over.
        env.ledger().set_timestamp(700_000);
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

        env.ledger().set_timestamp(700_000);
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

    // -------------------------------------------------------------------------
    // #1117: claim grace period, funding measurement, initialization
    // -------------------------------------------------------------------------

    /// Token that burns 2% of every transfer, to model a fee-on-transfer asset.
    mod fee_token {
        use soroban_sdk::{contract, contractimpl, contracttype, Address, Env};

        #[contracttype]
        enum Key {
            Bal(Address),
        }

        #[contract]
        pub struct FeeToken;

        fn bal(env: &Env, a: &Address) -> i128 {
            env.storage()
                .persistent()
                .get(&Key::Bal(a.clone()))
                .unwrap_or(0)
        }

        fn put(env: &Env, a: &Address, v: i128) {
            env.storage().persistent().set(&Key::Bal(a.clone()), &v);
        }

        #[contractimpl]
        impl FeeToken {
            pub fn mint(env: Env, to: Address, amount: i128) {
                let b = bal(&env, &to);
                put(&env, &to, b + amount);
            }

            pub fn balance(env: Env, id: Address) -> i128 {
                bal(&env, &id)
            }

            pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
                from.require_auth();
                let fee = amount * 2 / 100;
                let f = bal(&env, &from);
                assert!(f >= amount, "insufficient balance");
                put(&env, &from, f - amount);
                let t = bal(&env, &to);
                put(&env, &to, t + amount - fee);
            }
        }
    }
    use fee_token::{FeeToken, FeeTokenClient};

    const DAY: u64 = 86_400;

    #[test]
    fn test_create_campaign_uses_the_documented_default_grace() {
        let (env, incentives, pool, lp, reward, _p, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &5_000, &100, &400_000);
        assert_eq!(client.get_claim_grace(&id), DEFAULT_CLAIM_GRACE_SECS);
        assert_eq!(DEFAULT_CLAIM_GRACE_SECS, 7 * DAY);
    }

    #[test]
    fn test_create_campaign_with_grace_stores_the_requested_value() {
        let (env, incentives, pool, lp, reward, _p, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = client.create_campaign_with_grace(
            &gov, &pool, &lp, &reward, &1_000, &5_000, &100, &400_000, &7_200,
        );
        assert_eq!(client.get_claim_grace(&id), 7_200);
        assert_eq!(
            client.try_get_claim_grace(&99),
            Err(Ok(IncentiveError::CampaignNotFound))
        );
    }

    #[test]
    fn test_out_of_range_grace_is_rejected_and_boundaries_accepted() {
        let (env, incentives, pool, lp, reward, _p, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let try_with = |g: u64| {
            client.try_create_campaign_with_grace(
                &gov, &pool, &lp, &reward, &1_000, &5_000, &100, &400_000, &g,
            )
        };
        assert_eq!(
            try_with(MIN_CLAIM_GRACE_SECS - 1),
            Err(Ok(IncentiveError::InvalidClaimGrace))
        );
        assert_eq!(try_with(0), Err(Ok(IncentiveError::InvalidClaimGrace)));
        assert_eq!(
            try_with(MAX_CLAIM_GRACE_SECS + 1),
            Err(Ok(IncentiveError::InvalidClaimGrace))
        );
        assert_eq!(client.get_campaign_count(), 0);
        assert!(try_with(MIN_CLAIM_GRACE_SECS).is_ok());
        assert!(try_with(MAX_CLAIM_GRACE_SECS).is_ok());
    }

    #[test]
    fn test_recovery_is_blocked_until_end_plus_grace() {
        let (env, incentives, pool, lp, reward, _p, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let treasury = Address::generate(&env);
        let id = client.create_campaign_with_grace(
            &gov, &pool, &lp, &reward, &1_000, &5_000, &100, &400_000, &10_000,
        );

        env.ledger().set_timestamp(5_000);
        assert_eq!(
            client.try_recover_leftover_funds(&gov, &id, &treasury),
            Err(Ok(IncentiveError::CampaignNotEnded))
        );
        env.ledger().set_timestamp(5_001);
        assert_eq!(
            client.try_recover_leftover_funds(&gov, &id, &treasury),
            Err(Ok(IncentiveError::ClaimGraceActive))
        );
        env.ledger().set_timestamp(15_000);
        assert_eq!(
            client.try_recover_leftover_funds(&gov, &id, &treasury),
            Err(Ok(IncentiveError::ClaimGraceActive)),
            "end_time + grace itself is still inside the window"
        );
        assert!(
            client.get_campaign(&id).active,
            "a rejected recovery changes nothing"
        );

        env.ledger().set_timestamp(15_001);
        assert_eq!(client.recover_leftover_funds(&gov, &id, &treasury), 400_000);
        assert!(!client.get_campaign(&id).active);
    }

    #[test]
    fn test_providers_can_claim_during_the_grace_window() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let treasury = Address::generate(&env);
        let id = client.create_campaign_with_grace(
            &gov, &pool, &lp, &reward, &1_000, &5_000, &100, &400_000, &10_000,
        );
        make_legacy(&env, &incentives, id);
        assert_eq!(client.claim_rewards(&provider, &id), 0); // snapshot at t=1_000

        // The campaign has ended; recovery is blocked but the claim goes through.
        env.ledger().set_timestamp(9_000);
        assert_eq!(
            client.try_recover_leftover_funds(&gov, &id, &treasury),
            Err(Ok(IncentiveError::ClaimGraceActive))
        );
        let paid = client.claim_rewards(&provider, &id);
        assert!(paid > 0, "a claim inside the grace window pays out");

        env.ledger().set_timestamp(15_001);
        let left = client.recover_leftover_funds(&gov, &id, &treasury);
        assert_eq!(left, 400_000 - paid);
        assert_eq!(
            client.try_claim_rewards(&provider, &id),
            Err(Ok(IncentiveError::CampaignInactive)),
            "recovery after the grace window deactivates the campaign as before"
        );
    }

    #[test]
    fn test_legacy_campaign_without_a_stored_grace_recovers_as_before() {
        let (env, incentives, pool, lp, reward, _p, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let treasury = Address::generate(&env);
        let id = client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &5_000, &100, &400_000);
        // A campaign written before #1117 has no ClaimGraceSecs entry.
        env.as_contract(&incentives, || {
            env.storage()
                .persistent()
                .remove(&DataKey::ClaimGraceSecs(id));
        });
        assert_eq!(client.get_claim_grace(&id), 0);

        env.ledger().set_timestamp(5_000);
        assert_eq!(
            client.try_recover_leftover_funds(&gov, &id, &treasury),
            Err(Ok(IncentiveError::CampaignNotEnded))
        );
        env.ledger().set_timestamp(5_001);
        assert_eq!(client.recover_leftover_funds(&gov, &id, &treasury), 400_000);
    }

    #[test]
    fn test_fee_on_transfer_funding_at_exactly_the_payout_is_rejected() {
        let (env, incentives, pool, lp, _reward, _p, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let fee = env.register_contract(None, FeeToken);
        FeeTokenClient::new(&env, &fee).mint(&gov, &1_000_000);

        // 100/s over 4_000 s needs 400_000; the 2% fee leaves only 392_000.
        assert_eq!(
            client.try_create_campaign(&gov, &pool, &lp, &fee, &1_000, &5_000, &100, &400_000),
            Err(Ok(IncentiveError::InsufficientFunding))
        );
        assert_eq!(client.get_campaign_count(), 0);
        assert_eq!(FeeTokenClient::new(&env, &fee).balance(&gov), 1_000_000);
        assert_eq!(FeeTokenClient::new(&env, &fee).balance(&incentives), 0);
    }

    #[test]
    fn test_fee_on_transfer_funding_with_headroom_stores_the_received_amount() {
        let (env, incentives, pool, lp, _reward, _p, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let fee = env.register_contract(None, FeeToken);
        FeeTokenClient::new(&env, &fee).mint(&gov, &1_000_000);

        // 3% extra: 412_000 sent, 2% (8_240) burned, 403_760 received >= 400_000.
        let id = client.create_campaign(&gov, &pool, &lp, &fee, &1_000, &5_000, &100, &412_000);
        let received = FeeTokenClient::new(&env, &fee).balance(&incentives);
        assert_eq!(received, 403_760);
        assert_eq!(client.get_campaign(&id).funding_amount, received);
    }

    #[test]
    fn test_plain_token_funding_amount_equals_what_was_sent() {
        let (env, incentives, pool, lp, reward, _p, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &5_000, &100, &450_000);
        assert_eq!(client.get_campaign(&id).funding_amount, 450_000);
    }

    #[test]
    fn test_initialize_does_not_require_authorization() {
        let env = Env::default(); // deliberately no mock_all_auths
        let gov = Address::generate(&env);
        let id = env.register_contract(None, IncentiveCampaigns);
        let client = IncentiveCampaignsClient::new(&env, &id);
        client.initialize(&gov);
        assert!(
            env.auths().is_empty(),
            "initialize must not demand any signature"
        );
        assert_eq!(client.get_governance(), gov);
    }

    #[test]
    fn test_initialized_event_payload_equals_the_stored_config() {
        let env = Env::default();
        env.mock_all_auths();
        let gov = Address::generate(&env);
        let id = env.register_contract(None, IncentiveCampaigns);
        let client = IncentiveCampaignsClient::new(&env, &id);
        client.initialize(&gov);

        let (version, data): (u32, (Address, u64, u64)) =
            last_versioned_event(&env, &id, "initialized");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        let (g, next_campaign, next_distribution) = data;
        assert_eq!(g, client.get_governance());
        env.as_contract(&id, || {
            let s = env.storage().instance();
            assert_eq!(
                s.get::<DataKey, u64>(&DataKey::NextCampaignId),
                Some(next_campaign)
            );
            assert_eq!(
                s.get::<DataKey, u64>(&DataKey::NextDistributionId),
                Some(next_distribution)
            );
        });
    }

    #[test]
    fn test_get_governance_before_init_and_double_init_are_typed() {
        let env = Env::default();
        env.mock_all_auths();
        let gov = Address::generate(&env);
        let id = env.register_contract(None, IncentiveCampaigns);
        let client = IncentiveCampaignsClient::new(&env, &id);
        assert_eq!(
            client.try_get_governance(),
            Err(Ok(IncentiveError::NotInitialized))
        );
        client.initialize(&gov);
        assert_eq!(
            client.try_initialize(&Address::generate(&env)),
            Err(Ok(IncentiveError::AlreadyInitialized))
        );
        assert_eq!(
            client.get_governance(),
            gov,
            "a second initialize cannot replace governance"
        );
    }

    // -------------------------------------------------------------------------
    // #1116: append-only indexes, legacy fallback, migration
    // -------------------------------------------------------------------------

    type Views = (
        u32,
        soroban_sdk::Vec<DistributionRecord>,
        soroban_sdk::Vec<DistributionRecord>,
        soroban_sdk::Vec<DistributionRecord>,
        soroban_sdk::Vec<u64>,
        soroban_sdk::Vec<u64>,
    );

    fn views(
        client: &IncentiveCampaignsClient,
        id: u64,
        provider: &Address,
        creator: &Address,
    ) -> Views {
        (
            client.get_distribution_count(&id),
            client.list_distribution_records(&id, &0, &MAX_PAGE),
            client.list_distribution_records(&id, &1, &2),
            client.get_claim_history(provider, &0, &MAX_PAGE),
            client.get_campaigns_by_creator(creator, &0, &MAX_PAGE),
            client.get_campaigns_by_creator(creator, &1, &1),
        )
    }

    fn has_key(env: &Env, contract: &Address, key: &DataKey) -> bool {
        env.as_contract(contract, || env.storage().persistent().has(key))
    }

    /// Rewrite one index into the pre-#1116 `Vec` layout.
    fn to_legacy(env: &Env, contract: &Address, index: &Index) {
        env.as_contract(contract, || {
            let ck = index.count_key();
            let n: u32 = env.storage().persistent().get(&ck).unwrap();
            let mut v = soroban_sdk::Vec::<u64>::new(env);
            for i in 0..n {
                let k = index.item_key(i);
                v.push_back(env.storage().persistent().get::<DataKey, u64>(&k).unwrap());
                env.storage().persistent().remove(&k);
            }
            env.storage().persistent().remove(&ck);
            env.storage().persistent().set(&index.legacy_key(), &v);
        });
    }

    fn claim_at(
        env: &Env,
        client: &IncentiveCampaignsClient,
        provider: &Address,
        id: u64,
        t: u64,
    ) -> i128 {
        env.ledger().set_timestamp(t);
        client.claim_rewards(provider, &id)
    }

    /// Two campaigns, one provider, an interleaved claim history.
    /// Returns (id1, id2).
    fn interleaved_history(
        env: &Env,
        client: &IncentiveCampaignsClient,
        pool: &Address,
        lp: &Address,
        reward: &Address,
        provider: &Address,
        gov: &Address,
    ) -> (u64, u64) {
        let id1 = client.create_campaign(gov, pool, lp, reward, &1_000, &11_000, &100, &1_000_000);
        let id2 = client.create_campaign(gov, pool, lp, reward, &1_000, &11_000, &100, &1_000_000);
        make_legacy(env, &client.address, id1);
        make_legacy(env, &client.address, id2);
        claim_at(env, client, provider, id1, 1_000);
        claim_at(env, client, provider, id2, 1_000);
        assert!(claim_at(env, client, provider, id1, 2_000) > 0);
        assert!(claim_at(env, client, provider, id2, 2_500) > 0);
        assert!(claim_at(env, client, provider, id1, 3_000) > 0);
        assert!(claim_at(env, client, provider, id2, 3_500) > 0);
        assert!(claim_at(env, client, provider, id1, 4_000) > 0);
        (id1, id2)
    }

    #[test]
    fn test_claims_use_the_indexed_layout_and_leave_no_legacy_vectors() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let (id1, _id2) = interleaved_history(&env, &client, &pool, &lp, &reward, &provider, &gov);

        assert!(has_key(&env, &incentives, &DataKey::DistCampaignCount(id1)));
        assert!(has_key(
            &env,
            &incentives,
            &DataKey::DistCampaignItem(id1, 2)
        ));
        assert!(has_key(
            &env,
            &incentives,
            &DataKey::DistProviderCount(provider.clone())
        ));
        assert!(has_key(
            &env,
            &incentives,
            &DataKey::CreatorCount(gov.clone())
        ));
        assert!(!has_key(
            &env,
            &incentives,
            &DataKey::DistributionsByCampaign(id1)
        ));
        assert!(!has_key(
            &env,
            &incentives,
            &DataKey::DistributionsByProvider(provider.clone())
        ));
        assert!(!has_key(
            &env,
            &incentives,
            &DataKey::CampaignsByCreator(gov.clone())
        ));
        assert_eq!(client.get_distribution_count(&id1), 3);
    }

    #[test]
    fn test_views_are_identical_before_legacy_and_after_migration() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let (id1, id2) = interleaved_history(&env, &client, &pool, &lp, &reward, &provider, &gov);

        let golden1 = views(&client, id1, &provider, &gov);
        let golden2 = views(&client, id2, &provider, &gov);
        assert_eq!(golden1.0, 3);
        assert_eq!(golden2.0, 2);
        assert_eq!(golden1.3.len(), 5, "provider history spans both campaigns");
        assert_eq!(golden1.4.len(), 2);

        // Rewind every index to the legacy layout: views must not change.
        to_legacy(&env, &incentives, &Index::ByCampaign(id1));
        to_legacy(&env, &incentives, &Index::ByCampaign(id2));
        to_legacy(&env, &incentives, &Index::ByProvider(provider.clone()));
        to_legacy(&env, &incentives, &Index::ByCreator(gov.clone()));
        assert_eq!(views(&client, id1, &provider, &gov), golden1);
        assert_eq!(views(&client, id2, &provider, &gov), golden2);

        // Migrate everything: views still must not change.
        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_CAMPAIGN, &0, &MAX_PAGE),
            2
        );
        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_PROVIDER, &0, &MAX_PAGE),
            1
        );
        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_CREATOR, &0, &MAX_PAGE),
            1
        );
        assert_eq!(views(&client, id1, &provider, &gov), golden1);
        assert_eq!(views(&client, id2, &provider, &gov), golden2);
    }

    #[test]
    fn test_migration_moves_legacy_vectors_into_the_indexed_layout() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let (id1, id2) = interleaved_history(&env, &client, &pool, &lp, &reward, &provider, &gov);
        for idx in [
            Index::ByCampaign(id1),
            Index::ByCampaign(id2),
            Index::ByProvider(provider.clone()),
            Index::ByCreator(gov.clone()),
        ] {
            to_legacy(&env, &incentives, &idx);
        }
        assert!(has_key(
            &env,
            &incentives,
            &DataKey::DistributionsByCampaign(id1)
        ));
        assert!(!has_key(
            &env,
            &incentives,
            &DataKey::DistCampaignCount(id1)
        ));

        // Page through campaigns one at a time.
        assert_eq!(client.migrate_indexes(&INDEX_KIND_BY_CAMPAIGN, &0, &1), 1);
        assert!(has_key(&env, &incentives, &DataKey::DistCampaignCount(id1)));
        assert!(has_key(
            &env,
            &incentives,
            &DataKey::DistCampaignItem(id1, 2)
        ));
        assert!(!has_key(
            &env,
            &incentives,
            &DataKey::DistributionsByCampaign(id1)
        ));
        assert!(has_key(
            &env,
            &incentives,
            &DataKey::DistributionsByCampaign(id2)
        ));
        assert_eq!(client.migrate_indexes(&INDEX_KIND_BY_CAMPAIGN, &1, &1), 1);
        assert!(!has_key(
            &env,
            &incentives,
            &DataKey::DistributionsByCampaign(id2)
        ));

        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_PROVIDER, &0, &MAX_PAGE),
            1
        );
        assert!(!has_key(
            &env,
            &incentives,
            &DataKey::DistributionsByProvider(provider.clone())
        ));
        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_CREATOR, &0, &MAX_PAGE),
            1
        );
        assert!(!has_key(
            &env,
            &incentives,
            &DataKey::CampaignsByCreator(gov.clone())
        ));
        assert!(has_key(
            &env,
            &incentives,
            &DataKey::CreatorItem(gov.clone(), 1)
        ));
    }

    #[test]
    fn test_migration_is_idempotent() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let (id1, id2) = interleaved_history(&env, &client, &pool, &lp, &reward, &provider, &gov);
        let golden = views(&client, id1, &provider, &gov);

        // Already indexed: nothing to move.
        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_CAMPAIGN, &0, &MAX_PAGE),
            0
        );
        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_PROVIDER, &0, &MAX_PAGE),
            0
        );
        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_CREATOR, &0, &MAX_PAGE),
            0
        );

        to_legacy(&env, &incentives, &Index::ByCampaign(id1));
        to_legacy(&env, &incentives, &Index::ByCampaign(id2));
        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_CAMPAIGN, &0, &MAX_PAGE),
            2
        );
        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_CAMPAIGN, &0, &MAX_PAGE),
            0
        );
        assert_eq!(views(&client, id1, &provider, &gov), golden);
    }

    #[test]
    fn test_append_to_a_legacy_index_migrates_it_and_keeps_order() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let (id1, id2) = interleaved_history(&env, &client, &pool, &lp, &reward, &provider, &gov);
        let before = client.list_distribution_records(&id1, &0, &MAX_PAGE);
        let history_before = client.get_claim_history(&provider, &0, &MAX_PAGE);
        to_legacy(&env, &incentives, &Index::ByCampaign(id1));
        to_legacy(&env, &incentives, &Index::ByProvider(provider.clone()));
        let _ = id2;

        assert!(claim_at(&env, &client, &provider, id1, 5_000) > 0);

        assert!(!has_key(
            &env,
            &incentives,
            &DataKey::DistributionsByCampaign(id1)
        ));
        assert!(has_key(&env, &incentives, &DataKey::DistCampaignCount(id1)));
        let after = client.list_distribution_records(&id1, &0, &MAX_PAGE);
        assert_eq!(after.len(), before.len() + 1);
        for i in 0..before.len() {
            assert_eq!(after.get(i), before.get(i), "existing order is preserved");
        }
        let history_after = client.get_claim_history(&provider, &0, &MAX_PAGE);
        assert_eq!(history_after.len(), history_before.len() + 1);
        assert_eq!(
            history_after
                .get(history_after.len() - 1)
                .unwrap()
                .timestamp,
            5_000
        );
    }

    #[test]
    fn test_migrate_indexes_rejects_an_unknown_kind() {
        let (env, incentives, _pool, _lp, _reward, _p, _gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        assert_eq!(
            client.try_migrate_indexes(&3, &0, &10),
            Err(Ok(IncentiveError::InvalidIndexKind))
        );
        assert_eq!(
            client.try_migrate_indexes(&u32::MAX, &0, &10),
            Err(Ok(IncentiveError::InvalidIndexKind))
        );
    }

    #[test]
    fn test_migrate_indexes_pagination_bounds() {
        let (env, incentives, pool, lp, reward, _p, gov) = setup();
        env.budget().reset_unlimited();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let ids = seed_campaigns(&client, &gov, &pool, &lp, &reward, 55);
        env.as_contract(&incentives, || {
            for id in ids.iter() {
                let mut v = soroban_sdk::Vec::<u64>::new(&env);
                v.push_back(id);
                env.storage()
                    .persistent()
                    .set(&DataKey::DistributionsByCampaign(id), &v);
            }
        });

        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_CAMPAIGN, &0, &0),
            0,
            "limit 0"
        );
        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_CAMPAIGN, &55, &10),
            0,
            "offset at end"
        );
        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_CAMPAIGN, &999, &10),
            0,
            "offset past end"
        );
        // A huge limit is clamped to MAX_PAGE owners per call.
        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_CAMPAIGN, &0, &u32::MAX),
            MAX_PAGE
        );
        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_CAMPAIGN, &MAX_PAGE, &u32::MAX),
            5
        );
        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_CAMPAIGN, &0, &u32::MAX),
            0
        );
        assert_eq!(client.get_distribution_count(&1), 1);
        assert_eq!(client.get_distribution_count(&55), 1);
    }

    #[test]
    fn test_migrate_indexes_is_permissionless() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let (id1, _) = interleaved_history(&env, &client, &pool, &lp, &reward, &provider, &gov);
        to_legacy(&env, &incentives, &Index::ByCampaign(id1));
        assert_eq!(client.migrate_indexes(&INDEX_KIND_BY_CAMPAIGN, &0, &10), 1);
        assert!(
            env.auths().is_empty(),
            "migration must not require any signature"
        );
    }

    #[test]
    fn test_kind_one_finds_a_provider_through_its_records() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let (id1, id2) = interleaved_history(&env, &client, &pool, &lp, &reward, &provider, &gov);
        let golden = views(&client, id1, &provider, &gov);
        to_legacy(&env, &incentives, &Index::ByProvider(provider.clone()));

        // 5 records, all by one provider: one owner moved however many are scanned.
        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_PROVIDER, &0, &MAX_PAGE),
            1
        );
        assert_eq!(views(&client, id1, &provider, &gov), golden);
        let _ = id2;
    }

    #[test]
    fn test_kind_two_falls_back_to_governance_for_unrecorded_creators() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let (id1, id2) = interleaved_history(&env, &client, &pool, &lp, &reward, &provider, &gov);
        let golden = views(&client, id1, &provider, &gov);
        to_legacy(&env, &incentives, &Index::ByCreator(gov.clone()));
        // Campaigns created before #1117 have no recorded creator.
        env.as_contract(&incentives, || {
            env.storage()
                .persistent()
                .remove(&DataKey::CampaignCreator(id1));
            env.storage()
                .persistent()
                .remove(&DataKey::CampaignCreator(id2));
        });

        assert_eq!(
            client.migrate_indexes(&INDEX_KIND_BY_CREATOR, &0, &MAX_PAGE),
            1
        );
        assert!(!has_key(
            &env,
            &incentives,
            &DataKey::CampaignsByCreator(gov.clone())
        ));
        assert_eq!(views(&client, id1, &provider, &gov), golden);
    }

    #[test]
    fn test_index_entries_are_ttl_bumped_on_write_and_after_migration() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let (id1, _id2) = interleaved_history(&env, &client, &pool, &lp, &reward, &provider, &gov);

        let keys = [
            DataKey::DistCampaignCount(id1),
            DataKey::DistCampaignItem(id1, 0),
            DataKey::DistCampaignItem(id1, 2),
            DataKey::DistProviderCount(provider.clone()),
            DataKey::DistProviderItem(provider.clone(), 4),
            DataKey::CreatorCount(gov.clone()),
            DataKey::CreatorItem(gov.clone(), 1),
            DataKey::CampaignCreator(id1),
            DataKey::ClaimGraceSecs(id1),
        ];
        env.as_contract(&incentives, || {
            for k in keys.iter() {
                let ttl = env.storage().persistent().get_ttl(k);
                assert!(
                    ttl >= BUMP_TO - 1,
                    "index entry TTL {ttl} should be bumped to BUMP_TO"
                );
            }
        });

        // Views read the entries, which also refreshes them.
        let _ = client.list_distribution_records(&id1, &0, &10);
        let _ = client.get_claim_history(&provider, &0, &10);
        let _ = client.get_campaigns_by_creator(&gov, &0, &10);
        env.as_contract(&incentives, || {
            assert!(
                env.storage()
                    .persistent()
                    .get_ttl(&DataKey::DistCampaignItem(id1, 2))
                    >= BUMP_TO - 1
            );
        });

        // Entries written by a migration are bumped too.
        to_legacy(&env, &incentives, &Index::ByCampaign(id1));
        client.migrate_indexes(&INDEX_KIND_BY_CAMPAIGN, &0, &1);
        env.as_contract(&incentives, || {
            for k in [
                DataKey::DistCampaignCount(id1),
                DataKey::DistCampaignItem(id1, 2),
            ] {
                assert!(env.storage().persistent().get_ttl(&k) >= BUMP_TO - 1);
            }
        });
    }

    /// Cost of one claim at timestamp `t`, in CPU instructions.
    fn claim_cost(
        env: &Env,
        client: &IncentiveCampaignsClient,
        provider: &Address,
        id: u64,
        t: u64,
    ) -> u64 {
        env.ledger().set_timestamp(t);
        env.budget().reset_default();
        client.claim_rewards(provider, &id);
        env.budget().cpu_instruction_cost()
    }

    fn long_campaign(
        client: &IncentiveCampaignsClient,
        pool: &Address,
        lp: &Address,
        reward: &Address,
        gov: &Address,
        provider: &Address,
    ) -> u64 {
        // 100/s for 20_000 s, funded 2_000_000; the provider earns about 99/s.
        let id = client.create_campaign(gov, pool, lp, reward, &1_000, &21_000, &100, &2_000_000);
        make_legacy(&client.env, &client.address, id);
        client.claim_rewards(provider, &id); // snapshot at t=1_000, pays 0
        id
    }

    #[test]
    fn test_claim_cost_stays_flat_across_claims() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = long_campaign(&client, &pool, &lp, &reward, &gov, &provider);

        let first = claim_cost(&env, &client, &provider, id, 1_001);
        let mut last = first;
        for k in 2..=20u64 {
            let c = claim_cost(&env, &client, &provider, id, 1_000 + k);
            if k == 20 {
                last = c;
            }
        }
        assert_eq!(client.get_distribution_count(&id), 20);
        let diff = last.abs_diff(first);
        assert!(
            diff <= first / 2,
            "claim cost must stay relatively flat across claims (got {first} vs {last})"
        );
    }

    /// Prints the claim cost with 10 and with 5,000 prior claims.
    /// Run: cargo test -p incentive_campaigns report_claim_cost -- --ignored --nocapture
    #[test]
    #[ignore]
    fn report_claim_cost_at_10_and_5000_prior_claims() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = long_campaign(&client, &pool, &lp, &reward, &gov, &provider);
        let mut at_10 = 0;
        let mut at_5000 = 0;
        for k in 1..=5_001u64 {
            let c = claim_cost(&env, &client, &provider, id, 1_000 + k);
            if k == 11 {
                at_10 = c;
            }
            if k == 5_001 {
                at_5000 = c;
            }
        }
        std::println!("claim cost with 10 prior claims:    {at_10} cpu instructions");
        std::println!("claim cost with 5000 prior claims:  {at_5000} cpu instructions");
    }
    // =========================================================================
    // #1115: stake-based campaigns
    // =========================================================================

    fn set_time(env: &Env, t: u64) {
        env.ledger().with_mut(|l| l.timestamp = t);
    }

    fn mint_lp(env: &Env, lp: &Address, to: &Address, amount: i128) {
        LpTokenClient::new(env, lp).mint(to, &amount);
    }

    fn lp_balance(env: &Env, lp: &Address, who: &Address) -> i128 {
        LpTokenClient::new(env, lp).balance(who)
    }

    /// start=1_000, end=11_000, rate=100/s, funding=1_000_000 (exactly the max payout).
    fn std_campaign(
        client: &IncentiveCampaignsClient,
        gov: &Address,
        pool: &Address,
        lp: &Address,
        reward: &Address,
    ) -> u64 {
        client.create_campaign(gov, pool, lp, reward, &1_000, &11_000, &100, &1_000_000)
    }

    /// A passive provider that stayed staked for the whole window gets its
    /// exact share on the very first claim, however late (fails on main: 0).
    #[test]
    fn test_passive_provider_first_claim_pays_exact_share() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);

        client.stake(&provider, &id, &999_000);
        // Untouched until long after the campaign ended.
        set_time(&env, 500_000);
        let paid = client.claim_rewards(&provider, &id);
        assert!(
            (paid - 1_000_000).abs() <= 1,
            "sole staker for the whole window must get the whole budget, got {paid}"
        );
        assert_eq!(
            client.try_claim_rewards(&provider, &id),
            Err(Ok(IncentiveError::NoPendingRewards))
        );
    }

    /// A whale that stakes late cannot reprice the idle window (fails on main:
    /// 998 instead of roughly the whole idle-window reward).
    #[test]
    fn test_whale_cannot_reprice_an_idle_window() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);

        client.stake(&provider, &id, &999_000);

        // Nine thousand seconds pass untouched, then a whale 1_000x the pool
        // stakes and tries to claim.
        set_time(&env, 10_000);
        let whale = Address::generate(&env);
        mint_lp(&env, &lp, &whale, 999_000_000);
        client.stake(&whale, &id, &999_000_000);
        assert_eq!(
            client.try_claim_rewards(&whale, &id),
            Err(Ok(IncentiveError::NoPendingRewards))
        );

        set_time(&env, 11_000);
        let honest = client.claim_rewards(&provider, &id);
        // 9_000 s alone at 100/s = 900_000, plus ~0.1% of the last 1_000 s.
        assert!(honest >= 899_999, "idle window was repriced: {honest}");
        assert!(honest <= 900_200, "unexpectedly large payout: {honest}");
    }

    /// Stakes of 1:3 for the whole campaign split the budget 1:3.
    #[test]
    fn test_two_providers_split_one_to_three() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);
        let other = Address::generate(&env);
        mint_lp(&env, &lp, &other, 750_000);

        client.stake(&provider, &id, &250_000);
        client.stake(&other, &id, &750_000);
        assert_eq!(client.get_total_staked(&id), 1_000_000);

        set_time(&env, 11_000);
        assert_eq!(client.claim_rewards(&provider, &id), 250_000);
        assert_eq!(client.claim_rewards(&other, &id), 750_000);
    }

    /// A provider that is only staked for the last quarter earns exactly a
    /// quarter of the budget, and the idle three quarters are recoverable.
    #[test]
    fn test_last_quarter_staker_earns_a_quarter_and_idle_time_is_recoverable() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);
        let treasury = Address::generate(&env);

        set_time(&env, 8_500);
        client.stake(&provider, &id, &500_000);
        set_time(&env, 11_000);
        assert_eq!(client.claim_rewards(&provider, &id), 250_000);

        set_time(&env, 700_000);
        assert_eq!(
            client.recover_leftover_funds(&gov, &id, &treasury),
            750_000,
            "seconds with zero stake accrue to nobody"
        );
    }

    /// Staked rewards survive a full unstake and stay claimable; the idle time
    /// afterwards accrues to nobody.
    #[test]
    fn test_pending_is_kept_after_unstake_and_idle_time_goes_to_leftover() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);
        let treasury = Address::generate(&env);

        client.stake(&provider, &id, &400_000);
        set_time(&env, 3_000);
        client.unstake(&provider, &id, &400_000);
        assert_eq!(client.get_stake(&id, &provider), 0);
        assert_eq!(client.get_total_staked(&id), 0);
        assert_eq!(client.get_pending_rewards(&id, &provider), 200_000);

        set_time(&env, 9_000);
        assert_eq!(client.claim_rewards(&provider, &id), 200_000);

        set_time(&env, 700_000);
        assert_eq!(client.recover_leftover_funds(&gov, &id, &treasury), 800_000);
    }

    /// Each interval is priced at the rate in force over it.
    #[test]
    fn test_rate_change_prices_each_interval_at_its_own_rate() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id =
            client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &11_000, &100, &2_000_000);

        client.stake(&provider, &id, &999_000);
        set_time(&env, 3_000);
        client.set_campaign_rate(&gov, &id, &200);
        set_time(&env, 5_000);
        let paid = client.claim_rewards(&provider, &id);
        // 2_000 s at 100/s + 2_000 s at 200/s.
        assert!((paid - 600_000).abs() <= 2, "paid {paid}");
    }

    /// Another address staking, unstaking and claiming inside the window cannot
    /// change a staked provider's payout.
    #[test]
    fn test_third_party_activity_cannot_change_a_providers_payout() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);
        client.stake(&provider, &id, &999_000);

        set_time(&env, 6_000);
        let griefer = Address::generate(&env);
        mint_lp(&env, &lp, &griefer, 5_000_000);
        client.stake(&griefer, &id, &5_000_000);
        client.unstake(&griefer, &id, &5_000_000);
        let _ = client.try_claim_rewards(&griefer, &id);
        client.set_campaign_rate(&gov, &id, &100);

        set_time(&env, 11_000);
        let paid = client.claim_rewards(&provider, &id);
        assert!((paid - 1_000_000).abs() <= 2, "payout moved to {paid}");
    }

    #[test]
    fn test_stake_and_unstake_move_exact_lp_amounts() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);

        client.stake(&provider, &id, &600_000);
        assert_eq!(client.get_stake(&id, &provider), 600_000);
        assert_eq!(client.get_total_staked(&id), 600_000);
        assert_eq!(lp_balance(&env, &lp, &provider), 399_000);
        assert_eq!(lp_balance(&env, &lp, &incentives), 600_000);

        client.unstake(&provider, &id, &200_000);
        assert_eq!(client.get_stake(&id, &provider), 400_000);
        assert_eq!(client.get_total_staked(&id), 400_000);
        assert_eq!(lp_balance(&env, &lp, &provider), 599_000);

        client.unstake(&provider, &id, &400_000);
        assert_eq!(client.get_stake(&id, &provider), 0);
        assert_eq!(client.get_total_staked(&id), 0);
        assert_eq!(lp_balance(&env, &lp, &provider), 999_000);
        assert_eq!(lp_balance(&env, &lp, &incentives), 0);
    }

    #[test]
    fn test_stake_errors_are_typed() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);

        assert_eq!(
            client.try_stake(&provider, &id, &0),
            Err(Ok(IncentiveError::InvalidStakeAmount))
        );
        assert_eq!(
            client.try_stake(&provider, &99, &1),
            Err(Ok(IncentiveError::CampaignNotFound))
        );
        assert_eq!(
            client.try_unstake(&provider, &id, &1),
            Err(Ok(IncentiveError::NoStake))
        );
        assert_eq!(
            client.try_claim_rewards(&provider, &id),
            Err(Ok(IncentiveError::NoStake))
        );

        client.stake(&provider, &id, &1_000);
        assert_eq!(
            client.try_unstake(&provider, &id, &1_001),
            Err(Ok(IncentiveError::InsufficientStake))
        );
        assert_eq!(
            client.try_unstake(&provider, &id, &0),
            Err(Ok(IncentiveError::InvalidStakeAmount))
        );

        set_time(&env, 11_000);
        assert_eq!(
            client.try_stake(&provider, &id, &1),
            Err(Ok(IncentiveError::CampaignEnded))
        );
    }

    #[test]
    fn test_legacy_and_inactive_campaigns_reject_staking() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let treasury = Address::generate(&env);

        let legacy = std_campaign(&client, &gov, &pool, &lp, &reward);
        let modern = std_campaign(&client, &gov, &pool, &lp, &reward);
        make_legacy(&env, &incentives, legacy);

        assert!(!client.uses_staking(&legacy));
        assert!(client.uses_staking(&modern));
        assert!(!client.uses_staking(&99));
        assert_eq!(
            client.try_stake(&provider, &legacy, &1),
            Err(Ok(IncentiveError::LegacyCampaign))
        );
        assert_eq!(
            client.try_unstake(&provider, &legacy, &1),
            Err(Ok(IncentiveError::LegacyCampaign))
        );
        assert_eq!(
            client.try_get_pending_rewards(&legacy, &provider),
            Err(Ok(IncentiveError::LegacyCampaign))
        );

        // Once recovered, a campaign is inactive and takes no new stake.
        set_time(&env, 700_000);
        client.recover_leftover_funds(&gov, &modern, &treasury);
        assert_eq!(
            client.try_stake(&provider, &modern, &1),
            Err(Ok(IncentiveError::CampaignInactive))
        );
    }

    /// Principal is never held hostage: unstake works after the campaign was
    /// wound down, while claims are closed.
    #[test]
    fn test_unstake_still_works_after_leftover_recovery() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);
        let treasury = Address::generate(&env);

        client.stake(&provider, &id, &100_000);
        set_time(&env, 700_000);
        client.recover_leftover_funds(&gov, &id, &treasury);

        assert_eq!(
            client.try_claim_rewards(&provider, &id),
            Err(Ok(IncentiveError::CampaignInactive))
        );
        client.unstake(&provider, &id, &100_000);
        assert_eq!(lp_balance(&env, &lp, &provider), 999_000);
        assert_eq!(client.get_total_staked(&id), 0);
    }

    /// The read-only view reports exactly what the next claim pays.
    #[test]
    fn test_get_pending_rewards_matches_the_claim() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);

        assert_eq!(client.get_pending_rewards(&id, &provider), 0);
        client.stake(&provider, &id, &999_000);
        set_time(&env, 6_000);
        let view = client.get_pending_rewards(&id, &provider);
        assert!(view > 0);
        assert_eq!(client.claim_rewards(&provider, &id), view);
        assert_eq!(client.get_pending_rewards(&id, &provider), 0);
    }

    /// `get_campaign_accrual` stays consistent: capped at end_time and never
    /// below what was distributed, with stake changes checkpointing it too.
    #[test]
    fn test_accrual_audit_trail_stays_consistent_with_staking() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);

        client.stake(&provider, &id, &999_000);
        set_time(&env, 3_000);
        client.unstake(&provider, &id, &1_000);
        assert_eq!(client.get_campaign_accrual(&id), (200_000, 3_000));

        set_time(&env, 100_000);
        let paid = client.claim_rewards(&provider, &id);
        let (accrued, last) = client.get_campaign_accrual(&id);
        assert_eq!(accrued, 1_000_000, "capped at end_time");
        assert_eq!(last, 11_000);
        assert!(accrued >= client.get_campaign(&id).total_distributed);
        assert_eq!(client.get_campaign(&id).total_distributed, paid);
    }

    /// Staking before the campaign starts is allowed; rewards only flow from
    /// start_time.
    #[test]
    fn test_staking_before_start_accrues_from_start_time() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id =
            client.create_campaign(&gov, &pool, &lp, &reward, &5_000, &15_000, &100, &1_000_000);

        client.stake(&provider, &id, &999_000);
        assert_eq!(client.get_total_staked(&id), 999_000);
        set_time(&env, 7_000);
        let paid = client.claim_rewards(&provider, &id);
        assert!((paid - 200_000).abs() <= 1, "paid {paid}");
    }

    #[test]
    fn test_stake_and_unstake_emit_versioned_events() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);

        client.stake(&provider, &id, &1_000);
        let (version, data): (u32, (u64, Address, i128, i128)) =
            last_versioned_event(&env, &incentives, "staked");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(data, (id, provider.clone(), 1_000, 1_000));

        client.unstake(&provider, &id, &400);
        let (version, data): (u32, (u64, Address, i128, i128)) =
            last_versioned_event(&env, &incentives, "unstaked");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(data, (id, provider, 400, 600));
    }

    #[test]
    fn test_stake_entries_extend_ttl() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);
        client.stake(&provider, &id, &1_000);

        env.as_contract(&incentives, || {
            let marker = env.storage().persistent().get_ttl(&DataKey::StakeBased(id));
            let total = env
                .storage()
                .persistent()
                .get_ttl(&DataKey::CampaignTotalStaked(id));
            let position = env
                .storage()
                .persistent()
                .get_ttl(&DataKey::StakePosition(id, provider.clone()));
            assert!(marker >= BUMP_TO - 1, "StakeBased TTL {marker}");
            assert!(total >= BUMP_TO - 1, "CampaignTotalStaked TTL {total}");
            assert!(position >= BUMP_TO - 1, "StakePosition TTL {position}");
        });
    }

    /// Rounding dust never lets the campaign pay out more than it accrued, and
    /// whatever is not paid is recoverable.
    #[test]
    fn test_rounding_dust_never_overpays_and_is_recoverable() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);
        let b = Address::generate(&env);
        let c = Address::generate(&env);
        mint_lp(&env, &lp, &b, 333_334);
        mint_lp(&env, &lp, &c, 1);

        client.stake(&provider, &id, &333_333);
        client.stake(&b, &id, &333_334);
        client.stake(&c, &id, &1);

        set_time(&env, 50_000);
        let paid = client.claim_rewards(&provider, &id)
            + client.claim_rewards(&b, &id)
            + client
                .try_claim_rewards(&c, &id)
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or(0);
        assert!(paid <= 1_000_000, "overpaid: {paid}");
        assert!(
            paid >= 1_000_000 - 5,
            "lost more than rounding dust: {paid}"
        );

        // Recovery opens once the claim grace period has passed.
        set_time(&env, 700_000);
        let treasury = Address::generate(&env);
        let recovered = client.recover_leftover_funds(&gov, &id, &treasury);
        assert_eq!(paid + recovered, 1_000_000);
    }

    /// Raising the rate above what was funded cannot pay out more than the
    /// campaign holds.
    #[test]
    fn test_payout_is_capped_at_funding() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);

        client.stake(&provider, &id, &999_000);
        set_time(&env, 3_000);
        client.set_campaign_rate(&gov, &id, &300);
        set_time(&env, 20_000);
        assert_eq!(client.claim_rewards(&provider, &id), 1_000_000);
        assert_eq!(client.get_campaign(&id).total_distributed, 1_000_000);
        assert_eq!(
            soroban_sdk::token::Client::new(&env, &reward).balance(&incentives),
            0
        );
    }

    /// Hand-written sequence: stake / unstake / rate change / claims, then
    /// every claim plus the recovered leftover equals the funding.
    #[test]
    fn test_claims_plus_leftover_equal_funding_with_rate_change() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id =
            client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &11_000, &100, &2_000_000);
        let b = Address::generate(&env);
        mint_lp(&env, &lp, &b, 500_000);
        let mut paid = 0_i128;

        client.stake(&provider, &id, &300_000);
        set_time(&env, 2_000);
        client.stake(&b, &id, &500_000);
        set_time(&env, 4_000);
        client.stake(&provider, &id, &200_000);
        set_time(&env, 6_000);
        client.unstake(&b, &id, &200_000);
        set_time(&env, 7_000);
        client.set_campaign_rate(&gov, &id, &150);
        set_time(&env, 8_000);
        paid += client.claim_rewards(&provider, &id);
        set_time(&env, 9_000);
        client.unstake(&b, &id, &300_000);
        set_time(&env, 9_500);
        paid += client.claim_rewards(&b, &id);

        set_time(&env, 20_000);
        for who in [&provider, &b] {
            if let Ok(Ok(v)) = client.try_claim_rewards(who, &id) {
                paid += v;
            }
        }
        // Recovery opens once the claim grace period has passed.
        set_time(&env, 700_000);
        let treasury = Address::generate(&env);
        let recovered = client.recover_leftover_funds(&gov, &id, &treasury);
        assert_eq!(paid + recovered, 2_000_000);
        assert_eq!(client.get_campaign(&id).total_distributed, paid);
    }

    /// Property test over pseudo-random stake / unstake / claim / rate-change
    /// sequences: no tokens are created or stranded, and the contract's LP
    /// balance always equals the staked total.
    #[test]
    fn test_property_random_sequences_conserve_funding() {
        for seed in 1..=8_u64 {
            run_random_sequence(seed);
        }
    }

    fn next_rand(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state >> 33
    }

    fn run_random_sequence(seed: u64) {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        env.budget().reset_unlimited();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let reward_client = soroban_sdk::token::Client::new(&env, &reward);
        // Funds the highest rate used below (400/s over 10_000 s).
        let funding = 4_000_000_i128;
        let id = client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &11_000, &100, &funding);

        let second = Address::generate(&env);
        let third = Address::generate(&env);
        mint_lp(&env, &lp, &second, 500_000);
        mint_lp(&env, &lp, &third, 123_457);
        let users = [provider, second, third];

        let mut rng = seed;
        let mut now = 1_000_u64;
        let mut paid = 0_i128;
        for _ in 0..40 {
            now += next_rand(&mut rng) % 700;
            set_time(&env, now);
            let who = &users[(next_rand(&mut rng) % 3) as usize];
            match next_rand(&mut rng) % 5 {
                0 => {
                    let held = lp_balance(&env, &lp, who);
                    if held > 0 {
                        let amount = 1 + (next_rand(&mut rng) as i128) % held;
                        let _ = client.try_stake(who, &id, &amount);
                    }
                }
                1 => {
                    let staked = client.get_stake(&id, who);
                    if staked > 0 {
                        let amount = 1 + (next_rand(&mut rng) as i128) % staked;
                        client.unstake(who, &id, &amount);
                    }
                }
                2 => {
                    if let Ok(Ok(v)) = client.try_claim_rewards(who, &id) {
                        paid += v;
                    }
                }
                3 => {
                    let rate = 1 + (next_rand(&mut rng) as i128) % 400;
                    client.set_campaign_rate(&gov, &id, &rate);
                }
                _ => {}
            }

            let campaign = client.get_campaign(&id);
            assert_eq!(campaign.total_distributed, paid, "seed {seed}");
            assert_eq!(reward_client.balance(&incentives), funding - paid);
            assert_eq!(
                lp_balance(&env, &lp, &incentives),
                client.get_total_staked(&id),
                "seed {seed}: LP held by the contract must equal total staked"
            );
            assert!(client.get_campaign_accrual(&id).0 >= campaign.total_distributed);
        }

        // Wind down: everyone claims and exits, then governance recovers the rest
        // once the claim grace period has passed.
        set_time(&env, 700_000);
        for who in users.iter() {
            if let Ok(Ok(v)) = client.try_claim_rewards(who, &id) {
                paid += v;
            }
            let staked = client.get_stake(&id, who);
            if staked > 0 {
                client.unstake(who, &id, &staked);
            }
        }
        assert_eq!(client.get_total_staked(&id), 0);
        assert_eq!(lp_balance(&env, &lp, &incentives), 0);

        let treasury = Address::generate(&env);
        let recovered = match client.try_recover_leftover_funds(&gov, &id, &treasury) {
            Ok(Ok(v)) => v,
            _ => 0,
        };
        assert_eq!(paid + recovered, funding, "seed {seed}");
        assert_eq!(reward_client.balance(&incentives), 0, "seed {seed}");
    }

    /// Campaigns created before the upgrade keep the live-balance rules.
    #[test]
    fn test_legacy_campaign_keeps_original_first_claim_behaviour() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);
        make_legacy(&env, &incentives, id);

        assert_eq!(client.claim_rewards(&provider, &id), 0);
        set_time(&env, 6_000);
        assert!(client.claim_rewards(&provider, &id) > 0);
    }

    // ── Pause, Upgrade, and Policy Table Tests ─────────────────────────────────

    #[test]
    fn test_pause_policy_table_covers_every_public_function() {
        use std::collections::BTreeSet;
        use std::string::String;

        let source = include_str!("lib.rs");

        // 1. Extract table functions from module doc
        let mut table_functions = BTreeSet::new();
        let mut in_table = false;
        for line in source.lines() {
            if line.contains("# Pause policy") {
                in_table = true;
                continue;
            }
            if in_table {
                if line.starts_with("#![no_std]") || line.starts_with("use ") {
                    break;
                }
                let trimmed = line.trim();
                if trimmed.starts_with("//! | `") {
                    let rest = &trimmed["//! | `".len()..];
                    if let Some(end_idx) = rest.find('`') {
                        let fn_name = &rest[..end_idx];
                        table_functions.insert(String::from(fn_name));
                    }
                }
            }
        }

        // 2. Extract public functions from `impl IncentiveCampaigns`
        let mut contract_functions = BTreeSet::new();
        let mut in_impl = false;
        for line in source.lines() {
            if line.contains("impl IncentiveCampaigns {") {
                in_impl = true;
                continue;
            }
            if in_impl {
                if line.starts_with("}") {
                    break;
                }
                let trimmed = line.trim();
                if trimmed.starts_with("pub fn ") {
                    let rest = &trimmed["pub fn ".len()..];
                    if let Some(paren_idx) = rest.find('(') {
                        let fn_name = rest[..paren_idx].trim();
                        contract_functions.insert(String::from(fn_name));
                    }
                }
            }
        }

        assert!(
            !contract_functions.is_empty(),
            "Must have found public functions in IncentiveCampaigns"
        );
        assert!(
            !table_functions.is_empty(),
            "Must have found functions in pause policy table"
        );

        let missing_from_table: std::vec::Vec<_> =
            contract_functions.difference(&table_functions).collect();
        let extra_in_table: std::vec::Vec<_> =
            table_functions.difference(&contract_functions).collect();

        assert!(
            missing_from_table.is_empty(),
            "Public functions missing from the pause policy table: {:?}",
            missing_from_table
        );
        assert!(
            extra_in_table.is_empty(),
            "Extra functions in pause policy table not in contract: {:?}",
            extra_in_table
        );
    }

    #[test]
    fn test_pause_unpause_governance_only_and_emits_events() {
        let (env, incentives, _pool, _lp, _reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let stranger = Address::generate(&env);

        assert!(!client.is_paused());

        // Stranger cannot pause
        let res = client.try_pause(&stranger);
        assert_eq!(res, Err(Ok(IncentiveError::Unauthorized)));
        assert!(!client.is_paused());

        // Governance can pause
        client.pause(&gov);
        assert!(client.is_paused());

        let (version, (paused_by,)): (u32, (Address,)) =
            last_versioned_event(&env, &incentives, "paused");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(paused_by, gov);

        // Stranger cannot unpause
        let res = client.try_unpause(&stranger);
        assert_eq!(res, Err(Ok(IncentiveError::Unauthorized)));
        assert!(client.is_paused());

        // Governance can unpause
        client.unpause(&gov);
        assert!(!client.is_paused());

        let (version, (unpaused_by,)): (u32, (Address,)) =
            last_versioned_event(&env, &incentives, "unpaused");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(unpaused_by, gov);
    }

    #[test]
    fn test_create_campaign_blocked_while_paused() {
        let (env, incentives, pool, lp, reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        client.pause(&gov);

        let res =
            client.try_create_campaign(&gov, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000);
        assert_eq!(res, Err(Ok(IncentiveError::Paused)));

        client.unpause(&gov);
        let id =
            client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000);
        assert_eq!(id, 1);
    }

    #[test]
    fn test_set_campaign_rate_blocked_while_paused() {
        let (env, incentives, pool, lp, reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id =
            client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000);

        client.pause(&gov);

        let res = client.try_set_campaign_rate(&gov, &id, &150);
        assert_eq!(res, Err(Ok(IncentiveError::Paused)));

        client.unpause(&gov);
        client.set_campaign_rate(&gov, &id, &150);
        assert_eq!(client.get_campaign(&id).reward_rate, 150);
    }

    #[test]
    fn test_claim_rewards_blocked_while_paused() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id =
            client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000);

        client.stake(&provider, &id, &500_000);
        set_time(&env, 2_000);

        client.pause(&gov);

        let res = client.try_claim_rewards(&provider, &id);
        assert_eq!(res, Err(Ok(IncentiveError::Paused)));

        client.unpause(&gov);
        let claimed = client.claim_rewards(&provider, &id);
        assert!(claimed > 0);
    }

    #[test]
    fn test_accrual_unaffected_by_pause_twin_campaign_comparison() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        // Campaign 1: will be paused during t=2_000..3_000
        let id1 =
            client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000);

        // Provider stakes in Campaign 1
        client.stake(&provider, &id1, &500_000);

        // Advance to 2_000 and pause
        set_time(&env, 2_000);
        client.pause(&gov);

        // While paused, attempt claim and verify failure
        assert_eq!(
            client.try_claim_rewards(&provider, &id1),
            Err(Ok(IncentiveError::Paused))
        );

        // Advance to 3_000 while still paused
        set_time(&env, 3_000);

        // Unpause at 3_000
        client.unpause(&gov);

        // Advance to 4_000 and claim
        set_time(&env, 4_000);
        let payout1 = client.claim_rewards(&provider, &id1);

        // Now setup twin campaign under identical ledger timeline but without pause
        // (starts at 10_000, provider stakes at 10_000, claims at 13_000, identical 3_000s elapsed)
        let id2 = client.create_campaign(
            &gov, &pool, &lp, &reward, &10_000, &15_000, &100, &1_000_000,
        );
        set_time(&env, 10_000);
        client.stake(&provider, &id2, &500_000);
        set_time(&env, 13_000); // 3_000s elapsed, exactly matching t=1_000 to t=4_000 in id1
        let payout2 = client.claim_rewards(&provider, &id2);

        assert_eq!(
            payout1, payout2,
            "Claimant's accrued rewards after unpause must match an unpaused twin campaign advanced by the same ledgers"
        );
        assert_eq!(payout1, 300_000); // 100 reward/s * 3_000 s = 300_000
    }

    #[test]
    fn test_recover_leftover_funds_works_while_paused() {
        let (env, incentives, pool, lp, reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let treasury = Address::generate(&env);

        let id = client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &2_000, &100, &500_000);

        set_time(&env, 3_000); // Campaign ended
        client.pause(&gov);

        let recovered = client.recover_leftover_funds(&gov, &id, &treasury);
        assert_eq!(recovered, 500_000);
        assert!(!client.get_campaign(&id).active);
    }

    #[test]
    fn test_stake_and_unstake_work_while_paused() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id =
            client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000);

        client.pause(&gov);

        // Staking works while paused
        client.stake(&provider, &id, &200_000);
        assert_eq!(client.get_stake(&id, &provider), 200_000);
        assert_eq!(client.get_total_staked(&id), 200_000);

        // Unstaking works while paused (principal withdrawal is never held hostage)
        client.unstake(&provider, &id, &100_000);
        assert_eq!(client.get_stake(&id, &provider), 100_000);
        assert_eq!(client.get_total_staked(&id), 100_000);
    }

    #[test]
    fn test_views_work_while_paused() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id =
            client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000);
        client.stake(&provider, &id, &200_000);

        client.pause(&gov);

        assert!(client.is_paused());
        assert_eq!(client.version(), CONTRACT_VERSION);
        assert_eq!(client.get_governance(), gov.clone());
        assert_eq!(client.get_pending_governance(), None);
        assert_eq!(client.get_stake(&id, &provider), 200_000);
        assert_eq!(client.get_total_staked(&id), 200_000);
        assert!(client.uses_staking(&id));
        assert_eq!(client.get_campaign_count(), 1);
        assert_eq!(client.list_campaigns().len(), 1);
        assert_eq!(client.get_campaigns(&0, &10).len(), 1);
        assert_eq!(client.list_campaigns_paginated(&0, &10).len(), 1);
        assert_eq!(client.get_campaigns_paginated(&0, &10).len(), 1);
        assert_eq!(client.get_campaigns_by_creator(&gov, &0, &10).len(), 1);
        assert!(client.is_campaign_active(&id));
        assert_eq!(client.get_distribution_count(&id), 0);
        assert_eq!(client.list_distribution_records(&id, &0, &10).len(), 0);
        assert_eq!(client.get_claim_history(&provider, &0, &10).len(), 0);
        let camp = client.get_campaign(&id);
        assert_eq!(camp.id, id);
        let accrual = client.get_campaign_accrual(&id);
        assert_eq!(accrual.1, 1_000);
    }

    #[test]
    fn test_governance_rotation_works_while_paused() {
        let (env, incentives, _pool, _lp, _reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let new_gov = Address::generate(&env);

        client.pause(&gov);

        client.propose_governance(&gov, &new_gov);
        assert_eq!(client.get_pending_governance(), Some(new_gov.clone()));

        client.accept_governance(&new_gov);
        assert_eq!(client.get_governance(), new_gov.clone());
        assert_eq!(client.get_pending_governance(), None);

        // New governance can unpause
        client.unpause(&new_gov);
        assert!(!client.is_paused());
    }

    #[test]
    fn test_version_view() {
        let env = Env::default();
        env.mock_all_auths();
        let gov = Address::generate(&env);
        let incentives = env.register_contract(None, IncentiveCampaigns);
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        client.initialize(&gov);

        assert_eq!(client.version(), CONTRACT_VERSION);
        assert_eq!(client.version(), 1);
    }

    #[test]
    fn test_upgrade_requires_governance() {
        let (env, incentives, _pool, _lp, _reward, _provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let stranger = Address::generate(&env);
        let wasm_hash = env.deployer().upload_contract_wasm(WASM);

        let res = client.try_upgrade(&stranger, &wasm_hash);
        assert_eq!(res, Err(Ok(IncentiveError::Unauthorized)));

        // Governance call succeeds
        client.upgrade(&gov, &wasm_hash);
    }

    #[test]
    fn test_upgrade_preserves_campaigns_and_distribution_records_and_emits_upgraded() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);

        let id =
            client.create_campaign(&gov, &pool, &lp, &reward, &1_000, &5_000, &100, &1_000_000);
        client.stake(&provider, &id, &300_000);
        set_time(&env, 2_000);
        let claimed = client.claim_rewards(&provider, &id);
        assert_eq!(claimed, 100_000);
        assert_eq!(client.get_distribution_count(&id), 1);

        client.pause(&gov);

        // Upload same WASM and upgrade
        let wasm_hash = env.deployer().upload_contract_wasm(WASM);
        client.upgrade(&gov, &wasm_hash);

        // Verify upgraded event emitted
        let (version, (event_hash,)): (u32, (BytesN<32>,)) =
            last_versioned_event(&env, &incentives, "upgraded");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(event_hash, wasm_hash);

        // Verify state is preserved: campaign, distribution record, stake, pause status
        assert!(client.is_paused());
        assert_eq!(client.version(), CONTRACT_VERSION);
        let camp = client.get_campaign(&id);
        assert_eq!(camp.total_distributed, 100_000);
        assert_eq!(client.get_stake(&id, &provider), 300_000);
        assert_eq!(client.get_total_staked(&id), 300_000);
        assert_eq!(client.get_distribution_count(&id), 1);
        let rec = client.get_distribution_record(&1);
        assert_eq!(rec.amount, 100_000);
        assert_eq!(rec.provider, provider);

        // Unpause and further operations work seamlessly
        client.unpause(&gov);
        set_time(&env, 3_000);
        let claimed_next = client.claim_rewards(&provider, &id);
        assert_eq!(claimed_next, 100_000);
        assert_eq!(client.get_distribution_count(&id), 2);
    }

    #[test]
    fn test_legacy_campaign_claim_blocked_while_paused() {
        let (env, incentives, pool, lp, reward, provider, gov) = setup();
        let client = IncentiveCampaignsClient::new(&env, &incentives);
        let id = std_campaign(&client, &gov, &pool, &lp, &reward);
        make_legacy(&env, &incentives, id);

        assert_eq!(client.claim_rewards(&provider, &id), 0);
        set_time(&env, 6_000);

        client.pause(&gov);
        let res = client.try_claim_rewards(&provider, &id);
        assert_eq!(res, Err(Ok(IncentiveError::Paused)));

        client.unpause(&gov);
        assert!(client.claim_rewards(&provider, &id) > 0);
    }
}
