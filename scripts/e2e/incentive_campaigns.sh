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

  local campaign_id
  campaign_id=$(invoke "$campaigns" create_campaign \
    --caller "$governance" --pool "$pool" --lp_token "$lp" --reward_token "$reward" \
    --start_time "$start" --end_time "$end" --reward_rate "$rate" --funding_amount "$funding" | parse_i128)
  assert_eq "incentives: first campaign id" "$campaign_id" "1"
  assert_eq "incentives: campaign count" "$(invoke "$campaigns" get_campaign_count | parse_i128)" "1"
  assert_contains "incentives: campaign appears in page" "$(invoke "$campaigns" list_campaigns_paginated --offset 0 --limit 10)" "$campaign_id"
  assert_eq "incentives: escrow holds exact funding" "$(balance_of "$reward" "$campaigns")" "$funding"

  sleep 12
  assert_eq "incentives: campaign becomes active" "$(invoke "$campaigns" is_campaign_active --campaign_id "$campaign_id")" "true"
  invoke "$campaigns" claim_rewards --provider "$governance" --campaign_id "$campaign_id" >/dev/null
  sleep 6
  local payout
  payout=$(invoke "$campaigns" claim_rewards --provider "$governance" --campaign_id "$campaign_id" | parse_i128)
  assert_gt "incentives: second claim pays accrued rewards" "$payout" 0
  assert_eq "incentives: distribution count" "$(invoke "$campaigns" get_distribution_count --campaign_id "$campaign_id" | parse_i128)" "1"
  assert_contains "incentives: claim history contains campaign" "$(invoke "$campaigns" get_claim_history --provider "$governance" --offset 0 --limit 10)" "$campaign_id"
  assert_contains "incentives: distribution page contains campaign" "$(invoke "$campaigns" list_distribution_records --campaign_id "$campaign_id" --offset 0 --limit 10)" "$campaign_id"

  local accrued_before
  accrued_before=$(invoke "$campaigns" get_campaign_accrual --campaign_id "$campaign_id" | nth_num 1)
  invoke "$campaigns" set_campaign_rate --caller "$governance" --campaign_id "$campaign_id" --new_rate 5000 >/dev/null
  assert_eq "incentives: updated rate reads back" "$(invoke "$campaigns" get_campaign --campaign_id "$campaign_id" | json_num reward_rate)" "5000"
  assert_between "incentives: old-rate accrual is checkpointed" "$(invoke "$campaigns" get_campaign_accrual --campaign_id "$campaign_id" | nth_num 1)" "$accrued_before" "$funding"

  local wait_for_end=$(( end - $(date +%s) + 6 ))
  if (( wait_for_end > 0 )); then sleep "$wait_for_end"; fi
  invoke "$campaigns" claim_rewards --provider "$governance" --campaign_id "$campaign_id" >/dev/null
  local distributed expected_leftover recovered
  distributed=$(invoke "$campaigns" get_campaign --campaign_id "$campaign_id" | json_num total_distributed)
  expected_leftover=$(( funding - distributed ))
  recovered=$(invoke "$campaigns" recover_leftover_funds --caller "$governance" --campaign_id "$campaign_id" --recipient "$governance" | parse_i128)
  assert_eq "incentives: recovery returns exact remainder" "$recovered" "$expected_leftover"
  assert_eq "incentives: recovery deactivates campaign" "$(invoke "$campaigns" get_campaign --campaign_id "$campaign_id" | grep -Eo '"active":[[:space:]]*(true|false)' | cut -d: -f2 | tr -d ' ')" "false"
  expect_fail "incentives: second recovery is rejected" invoke "$campaigns" recover_leftover_funds --caller "$governance" --campaign_id "$campaign_id" --recipient "$governance"
  expect_fail "incentives: claims after recovery are rejected" invoke "$campaigns" claim_rewards --provider "$governance" --campaign_id "$campaign_id"

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
