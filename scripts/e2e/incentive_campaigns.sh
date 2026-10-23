#!/usr/bin/env bash
# incentive_campaigns.sh - live RPC lifecycle for campaign funding and claims.
set -Eeuo pipefail

run_incentive_campaigns_flow() {
  CURRENT_FLOW="incentive_campaigns"
  local governance="$SOURCE_PUBLIC_KEY"
  local outsider_key="${SOURCE_ACCOUNT}-campaign-outsider"
  local successor_key="${SOURCE_ACCOUNT}-campaign-successor"
  local outsider successor
  outsider=$(e2e_fund_account campaign-outsider)
  successor=$(e2e_fund_account campaign-successor)

  local token_a token_b reward factory pool lp campaigns
  token_a=$(e2e_new_token ICA)
  token_b=$(e2e_new_token ICB)
  reward=$(e2e_new_token ICR)
  factory=$(e2e_new_factory)
  pool=$(e2e_new_seeded_pool "$factory" "$token_a" "$token_b" 10000000)
  lp=$(invoke "$factory" get_lp_token --pool "$pool" | parse_address)
  campaigns=$(e2e_deploy incentive_campaigns)

  invoke "$campaigns" initialize --governance "$governance" >/dev/null
  assert_eq "incentives: governance reads back" "$(invoke "$campaigns" get_governance | parse_address)" "$governance"
  expect_fail "incentives: second initialize is rejected" invoke "$campaigns" initialize --governance "$governance"

  local start end rate funding
  start=$(( $(date +%s) + 10 ))
  end=$(( start + 36 ))
  rate=10000
  funding=$(( rate * (end - start) ))
  invoke "$reward" mint --to "$governance" --amount "$funding" >/dev/null

  expect_fail "incentives: end <= start is rejected" invoke "$campaigns" create_campaign \
    --caller "$governance" --pool "$pool" --lp_token "$lp" --reward_token "$reward" \
    --start_time "$start" --end_time "$start" --reward_rate "$rate" --funding_amount "$funding"
  expect_fail "incentives: underfunded campaign is rejected" invoke "$campaigns" create_campaign \
    --caller "$governance" --pool "$pool" --lp_token "$lp" --reward_token "$reward" \
    --start_time "$start" --end_time "$end" --reward_rate "$rate" --funding_amount "$(( funding - 1 ))"
  expect_fail "incentives: non-governance caller is rejected" invoke_as "$outsider_key" "$campaigns" create_campaign \
    --caller "$outsider" --pool "$pool" --lp_token "$lp" --reward_token "$reward" \
    --start_time "$start" --end_time "$end" --reward_rate "$rate" --funding_amount "$funding"
  expect_fail "incentives: LP token admin must be the pool" invoke "$campaigns" create_campaign \
    --caller "$governance" --pool "$pool" --lp_token "$reward" --reward_token "$reward" \
    --start_time "$start" --end_time "$end" --reward_rate "$rate" --funding_amount "$funding"

  # The window is computed now, not before the rejection checks above: each of
  # those transactions takes several seconds, and a window fixed up front can be
  # over before the campaign is created.
  start=$(( $(date +%s) + 20 ))
  end=$(( start + 36 ))
  local campaign_id
  campaign_id=$(invoke "$campaigns" create_campaign \
    --caller "$governance" --pool "$pool" --lp_token "$lp" --reward_token "$reward" \
    --start_time "$start" --end_time "$end" --reward_rate "$rate" --funding_amount "$funding" | parse_i128)
  assert_eq "incentives: first campaign id" "$campaign_id" "1"
  assert_eq "incentives: campaign count" "$(invoke "$campaigns" get_campaign_count | parse_i128)" "1"
  assert_contains "incentives: campaign appears in page" "$(invoke "$campaigns" list_campaigns_paginated --offset 0 --limit 10)" "$campaign_id"
  assert_eq "incentives: escrow holds exact funding" "$(balance_of "$reward" "$campaigns")" "$funding"
  assert_eq "incentives: campaign uses the default claim grace" "$(invoke "$campaigns" get_claim_grace --campaign_id "$campaign_id" | parse_i128)" "604800"

  # New campaigns pay stakers only, so the provider stakes LP shares first.
  # Staking before start_time is allowed; rewards begin flowing at start_time.
  local lp_balance stake_amount
  lp_balance=$(balance_of "$lp" "$governance")
  stake_amount=$(( lp_balance / 2 ))
  assert_gt "incentives: provider holds LP shares to stake" "$stake_amount" 0
  invoke "$campaigns" stake --provider "$governance" --campaign_id "$campaign_id" --amount "$stake_amount" >/dev/null
  assert_eq "incentives: stake reads back" "$(invoke "$campaigns" get_stake --campaign_id "$campaign_id" --provider "$governance" | parse_i128)" "$stake_amount"
  assert_eq "incentives: staked LP is held by the contract" "$(balance_of "$lp" "$campaigns")" "$stake_amount"

  e2e_wait_until $(( start + 14 ))
  assert_eq "incentives: campaign is active inside its window" "$(invoke "$campaigns" is_campaign_active --campaign_id "$campaign_id")" "true"
  expect_fail "incentives: claim without a stake is rejected" \
    invoke_as "$outsider_key" "$campaigns" claim_rewards --provider "$outsider" --campaign_id "$campaign_id"
  local payout
  payout=$(invoke "$campaigns" claim_rewards --provider "$governance" --campaign_id "$campaign_id" | parse_i128)
  assert_gt "incentives: first claim pays accrued rewards" "$payout" 0
  assert_eq "incentives: distribution count" "$(invoke "$campaigns" get_distribution_count --campaign_id "$campaign_id" | parse_i128)" "1"
  assert_contains "incentives: claim history contains campaign" "$(invoke "$campaigns" get_claim_history --provider "$governance" --offset 0 --limit 10)" "$campaign_id"
  assert_contains "incentives: distribution page contains campaign" "$(invoke "$campaigns" list_distribution_records --campaign_id "$campaign_id" --offset 0 --limit 10)" "$campaign_id"

  local accrued_before
  accrued_before=$(invoke "$campaigns" get_campaign_accrual --campaign_id "$campaign_id" | nth_num 1)
  invoke "$campaigns" set_campaign_rate --caller "$governance" --campaign_id "$campaign_id" --new_rate 5000 >/dev/null
  assert_eq "incentives: updated rate reads back" "$(invoke "$campaigns" get_campaign --campaign_id "$campaign_id" | json_num reward_rate)" "5000"
  assert_between "incentives: old-rate accrual is checkpointed" "$(invoke "$campaigns" get_campaign_accrual --campaign_id "$campaign_id" | nth_num 1)" "$accrued_before" "$funding"

  e2e_wait_until $(( end + 6 ))
  local final_payout distributed
  final_payout=$(invoke "$campaigns" claim_rewards --provider "$governance" --campaign_id "$campaign_id" | parse_i128)
  assert_gt "incentives: claim after the campaign ends pays the remainder" "$final_payout" 0
  distributed=$(invoke "$campaigns" get_campaign --campaign_id "$campaign_id" | json_num total_distributed)
  assert_eq "incentives: total distributed equals the claims" "$distributed" "$(( payout + final_payout ))"
  assert_between "incentives: distributed stays within the funding" "$distributed" 1 "$funding"

  # Leftover recovery is blocked until end_time + claim_grace_secs (at least an
  # hour), which a live-network run cannot wait for; the positive path is covered
  # by the contract's unit tests.
  expect_fail "incentives: recovery inside the claim grace window is rejected" \
    invoke "$campaigns" recover_leftover_funds --caller "$governance" --campaign_id "$campaign_id" --recipient "$governance"

  invoke "$campaigns" unstake --provider "$governance" --campaign_id "$campaign_id" --amount "$stake_amount" >/dev/null
  assert_eq "incentives: unstake clears the position" "$(invoke "$campaigns" get_stake --campaign_id "$campaign_id" --provider "$governance" | parse_i128)" "0"
  assert_eq "incentives: unstake returns the LP shares" "$(balance_of "$lp" "$governance")" "$lp_balance"

  invoke "$campaigns" propose_governance --caller "$governance" --new_governance "$successor" >/dev/null
  invoke_as "$successor_key" "$campaigns" accept_governance --new_governance "$successor" >/dev/null
  assert_eq "incentives: governance handover reads back" "$(invoke "$campaigns" get_governance | parse_address)" "$successor"
  expect_fail "incentives: former governance loses create authority" invoke "$campaigns" create_campaign \
    --caller "$governance" --pool "$pool" --lp_token "$lp" --reward_token "$reward" \
    --start_time "$start" --end_time "$end" --reward_rate "$rate" --funding_amount "$funding"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
  e2e_standalone run_incentive_campaigns_flow
fi
