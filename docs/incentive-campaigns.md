# Incentive campaigns: stake-based rewards and migration

Contract: [`contracts/incentive_campaigns`](../contracts/incentive_campaigns/src/lib.rs).
Errors: [error-codes.md](error-codes.md#incentivecampaigns-contractsincentive_campaigns).
Events: [event-schema-versioning.md](event-schema-versioning.md).

## Why the model changed (issue #1115)

Campaigns used to price every second of the campaign from data the contract
cannot observe over time: the LP token's live `total_supply` and each provider's
live LP balance. Two things followed:

- A provider was only registered by their first `claim_rewards`, so a passive
  provider's first claim paid 0 however long they had held LP tokens.
- An idle window could be repriced by whoever touched the campaign next, by
  temporarily changing the LP supply right before the touch.

## The stake-based model

Campaigns created after the upgrade are *stake-based*. Providers move LP tokens
into the contract with `stake` and take them out with `unstake`, so the contract
sees every change to the staked amount when it happens.

| Function | Who | What it does |
|---|---|---|
| `stake(provider, campaign_id, amount)` | provider | Settles accrual, then escrows `amount` LP tokens. Allowed before `start_time`; rejected at or after `end_time` (`CampaignEnded`). |
| `unstake(provider, campaign_id, amount)` | provider | Settles accrual, then returns `amount` LP tokens. Always available, including after the campaign ended or was wound down. Earned rewards stay on the position. |
| `claim_rewards(provider, campaign_id)` | provider | Pays `pending + amount * (acc_now - acc_at_snapshot) / PRECISION`. Works after `end_time` for what accrued up to `end_time`. |
| `get_stake(campaign_id, provider)` | anyone | The provider's staked amount. |
| `get_total_staked(campaign_id)` | anyone | The campaign's total stake. |
| `get_pending_rewards(campaign_id, provider)` | anyone | What `claim_rewards` would pay right now. |
| `uses_staking(campaign_id)` | anyone | `true` for stake-based campaigns, `false` for legacy ones. |

Accounting rules:

- The accumulator advances by `reward_rate * dt * PRECISION / total_staked`,
  where `total_staked` is the campaign's own counter. Only `stake` and `unstake`
  change it, so each interval is priced at the stake that existed over it.
- `stake`, `unstake` and `set_campaign_rate` advance the accumulator to "now"
  before they change anything. Nobody else can reprice another provider's
  interval.
- A position staked for the whole window receives its exact share on its first
  claim, however late it is made.
- Seconds with zero stake accrue to nobody. They stay in
  `funding_amount - total_distributed`, which `recover_leftover_funds` returns.
- Integer division rounds down, so the sum of payouts never exceeds the funding.
  The rounding dust stays in the leftover.

### Behaviour to be aware of

- `recover_leftover_funds` marks the campaign inactive, after which
  `claim_rewards` fails with `CampaignInactive`. Rewards accrued but not yet
  claimed at that point are part of the recovered leftover. The contract
  enforces the grace period: recovery fails with `ClaimGraceActive` until the
  ledger time is strictly past `end_time + claim_grace_secs` (7 days by
  default; `create_campaign_with_grace` accepts 1 hour to 90 days), and
  providers keep claiming throughout it.
  `unstake` keeps working, so principal is never lost.
- `claim_rewards` by an address with no stake and nothing accrued fails with
  `NoStake`. An address with a stake but nothing new to collect gets
  `NoPendingRewards`.

## Migrating campaigns that existed before the upgrade

Storage is append-only: the new `DataKey` variants (`StakeBased`,
`CampaignTotalStaked`, `StakePosition`) were added at the end, so no existing key
changes meaning.

A campaign is stake-based only if it carries the `StakeBased(campaign_id)`
marker, which `create_campaign` writes. Campaigns created before the upgrade have
no marker and keep the *old* rules, unchanged:

- `claim_rewards` dispatches to the original live-balance logic;
- the first call registers the provider and pays 0;
- later calls pay from the provider's live LP balance;
- `stake` / `unstake` on such a campaign fail with `LegacyCampaign`.

Existing funds and distribution records are untouched, so an upgrade cannot change
what a running legacy campaign owes.

To move liquidity providers onto the fixed model:

1. Upgrade the contract. Nothing changes for running campaigns.
2. Let each legacy campaign finish. Providers claim what they are owed in the
   normal way, with the old caveat that the first claim pays 0.
3. After the grace period, governance calls `recover_leftover_funds` on the
   legacy campaign.
4. Create the replacement campaign with `create_campaign`. It is stake-based
   automatically. Announce that providers must `stake` their LP tokens into it.
5. Clients must call `stake` before `claim_rewards` on new campaigns, and use
   `uses_staking(campaign_id)` to choose the flow per campaign.

There is no in-place conversion of a running legacy campaign. Switching its
pricing mid-flight would re-price seconds that have already been accrued under the
old rules, which is the flaw this change removes.

## Integrator checklist

- Call `uses_staking` before offering a campaign's UI.
- For stake-based campaigns, approve and call `stake`; show `get_pending_rewards`
  instead of estimating from the LP balance.
- Providers who only hold LP tokens in their wallet earn nothing from a
  stake-based campaign. This is intended: the contract can only price what it can
  observe.
