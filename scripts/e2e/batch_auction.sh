#!/usr/bin/env bash
# batch_auction.sh — end-to-end flow for the batch auction on a live network:
# the venue registry, order intake, cancellation and its rejections, expiry,
# a timed settlement, an order that cannot be filled, and admin rotation.
#
# Every state change is read back through a view, and every rejected call is
# checked with expect_fail. Steps that depend on ledger time read the close
# time from the RPC node (e2e_wait_for_ledger_time), not the wall clock.
#
# Self-contained: deploys its own tokens, factory, pool and auction, so it runs
# without deploy.sh:
#   bash scripts/e2e/batch_auction.sh
set -Eeuo pipefail

run_batch_auction_flow() {
  CURRENT_FLOW="batch_auction"

  local admin="$SOURCE_PUBLIC_KEY"
  # The window must outlast the setup steps before settlement, and the flow
  # waits for it to close, so keep it short.
  local window="${BATCH_WINDOW_SECS:-90}"
  local seed="${BATCH_POOL_SEED:-10000000}"
  local trader_pay=1000000
  local t1_key="${SOURCE_ACCOUNT}-auction-t1"
  local t2_key="${SOURCE_ACCOUNT}-auction-t2"
  local new_admin_key="${SOURCE_ACCOUNT}-auction-admin2"

  # ── fixtures: tokens, factory, attested pool, two traders ──────────────
  local ta tb factory pool t1 t2 new_admin
  ta=$(e2e_new_token BAA)
  tb=$(e2e_new_token BAB)
  factory=$(e2e_new_factory)
  pool=$(e2e_new_seeded_pool "$factory" "$ta" "$tb" "$seed")
  t1=$(e2e_fund_account auction-t1)
  t2=$(e2e_fund_account auction-t2)
  new_admin=$(e2e_fund_account auction-admin2)
  invoke "$ta" mint --to "$t1" --amount "$trader_pay" >/dev/null
  invoke "$tb" mint --to "$t2" --amount "$trader_pay" >/dev/null
  invoke "$ta" mint --to "$t2" --amount "$trader_pay" >/dev/null
  pass "batch_auction: fixtures ready (pool=$pool)"

  # ── initialize ──────────────────────────────────────────────────────────
  local auction
  auction=$(e2e_deploy batch_auction)
  invoke "$auction" initialize --admin "$admin" --batch_window_secs "$window" >/dev/null
  local info
  info=$(invoke "$auction" get_batch_info)
  assert_eq "batch_auction: starts with no pending orders" "$(printf '%s\n' "$info" | nth_num 1)" "0"
  assert_eq "batch_auction: batch window reads back" "$(printf '%s\n' "$info" | nth_num 4)" "$window"
  assert_eq "batch_auction: get_admin reads back" "$(invoke "$auction" get_admin | parse_address)" "$admin"
  expect_fail "batch_auction: second initialize is rejected" \
    invoke "$auction" initialize --admin "$admin" --batch_window_secs "$window"

  # The window opened at initialize, so settling now must be rejected.
  expect_fail "batch_auction: settle before the window closes is rejected (BatchWindowOpen)" \
    invoke "$auction" settle_batch
  invoke "$auction" set_factory --admin "$admin" --factory "$factory" >/dev/null

  # ── venue registry ──────────────────────────────────────────────────────
  # A pool that the factory did not create and the admin has not allowlisted.
  local raw_pool lp_raw
  lp_raw=$(e2e_new_token BAL)
  raw_pool=$(e2e_deploy amm)
  invoke "$raw_pool" initialize --admin "$admin" --token_a "$ta" --token_b "$tb" \
    --lp_token "$lp_raw" --fee_bps 30 --fee_recipient "$admin" --protocol_fee_bps 0 >/dev/null
  expect_fail "batch_auction: order on a pool that is neither attested nor allowlisted is rejected (UnknownVenue)" \
    invoke_as "$t1_key" "$auction" submit_order \
      --trader "$t1" --pool "$raw_pool" --token_in "$ta" --token_out "$tb" \
      --amount_in 1000 --min_out 1 --deadline "$(( $(e2e_ledger_time) + 3600 ))"

  invoke "$auction" add_venue --admin "$admin" --pool "$raw_pool" --pool_type Amm >/dev/null
  assert_eq "batch_auction: add_venue allowlists the pool" \
    "$(invoke "$auction" is_venue_allowed --pool "$raw_pool")" "true"
  assert_contains "batch_auction: list_venues includes the allowlisted pool" \
    "$(invoke "$auction" list_venues --offset 0 --limit 50)" "$raw_pool"
  invoke "$auction" remove_venue --admin "$admin" --pool "$raw_pool" >/dev/null
  assert_eq "batch_auction: remove_venue removes the pool" \
    "$(invoke "$auction" is_venue_allowed --pool "$raw_pool")" "false"

  # ── order intake: two traders, escrow ──────────────────────────────────
  local now deadline_far
  now=$(e2e_ledger_time)
  local amt1=1000 amt2=1000 amt3=500 amt4=200 amt5=300
  local order1 order2
  order1=$(invoke_as "$t1_key" "$auction" submit_order \
    --trader "$t1" --pool "$pool" --token_in "$ta" --token_out "$tb" \
    --amount_in "$amt1" --min_out 1 --deadline "$(( now + 3600 ))" | parse_i128)
  order2=$(invoke_as "$t2_key" "$auction" submit_order \
    --trader "$t2" --pool "$pool" --token_in "$tb" --token_out "$ta" \
    --amount_in "$amt2" --min_out 1 --deadline "$(( now + 3600 ))" | parse_i128)
  assert_eq "batch_auction: escrow holds trader 1's input" "$(balance_of "$ta" "$auction")" "$amt1"
  assert_eq "batch_auction: escrow holds trader 2's input" "$(balance_of "$tb" "$auction")" "$amt2"
  assert_eq "batch_auction: two orders pending" \
    "$(invoke "$auction" get_batch_info | nth_num 1)" "2"

  # Trader 1 places and cancels a third order: the refund is exact.
  local t1_before_order3 order3
  t1_before_order3=$(balance_of "$ta" "$t1")
  order3=$(invoke_as "$t1_key" "$auction" submit_order \
    --trader "$t1" --pool "$pool" --token_in "$ta" --token_out "$tb" \
    --amount_in "$amt3" --min_out 1 --deadline "$(( now + 3600 ))" | parse_i128)
  invoke_as "$t1_key" "$auction" cancel_order --trader "$t1" --order_id "$order3" >/dev/null
  assert_eq "batch_auction: cancel refunds the escrow exactly" \
    "$(balance_of "$ta" "$t1")" "$t1_before_order3"
  assert_eq "batch_auction: cancelled order leaves the batch" \
    "$(invoke "$auction" get_batch_info | nth_num 1)" "2"
  expect_fail "batch_auction: cancelling someone else's order is rejected" \
    invoke_as "$t2_key" "$auction" cancel_order --trader "$t2" --order_id "$order1"

  deadline_far=$(( now + 8 * 24 * 3600 ))
  expect_fail "batch_auction: a deadline beyond the maximum order lifetime is rejected (DeadlineTooFar)" \
    invoke_as "$t1_key" "$auction" submit_order \
      --trader "$t1" --pool "$pool" --token_in "$ta" --token_out "$tb" \
      --amount_in "$amt3" --min_out 1 --deadline "$deadline_far"

  invoke "$auction" set_max_orders --admin "$admin" --n 2 >/dev/null
  expect_fail "batch_auction: a full batch rejects new orders (BatchFull)" \
    invoke_as "$t1_key" "$auction" submit_order \
      --trader "$t1" --pool "$pool" --token_in "$ta" --token_out "$tb" \
      --amount_in "$amt3" --min_out 1 --deadline "$(( now + 3600 ))"
  invoke "$auction" set_max_orders --admin "$admin" --n 50 >/dev/null

  # ── expiry: an order whose deadline passes before settlement ───────────
  local t1_before_order4 order4
  t1_before_order4=$(balance_of "$ta" "$t1")
  # Read the clock at submission: the deadline must still be ahead of it.
  local deadline4=$(( $(e2e_ledger_time) + 45 ))
  order4=$(invoke_as "$t1_key" "$auction" submit_order \
    --trader "$t1" --pool "$pool" --token_in "$ta" --token_out "$tb" \
    --amount_in "$amt4" --min_out 1 --deadline "$deadline4" | parse_i128)
  # An order no pool can fill: min_out is far above what the pool returns.
  invoke_as "$t1_key" "$auction" submit_order \
    --trader "$t1" --pool "$pool" --token_in "$ta" --token_out "$tb" \
    --amount_in "$amt5" --min_out 1000000000000 --deadline "$(( now + 3600 ))" >/dev/null
  assert_eq "batch_auction: four live orders before expiry" \
    "$(invoke "$auction" get_batch_info | nth_num 1)" "4"

  e2e_wait_for_ledger_time "$(( deadline4 + 1 ))" "batch_auction: order 4 deadline"
  local expired
  expired=$(invoke "$auction" get_expired_orders)
  assert_eq "batch_auction: only the timed-out order is expired" \
    "$(printf '%s\n' "$expired" | count_matches '[0-9]+')" "1"
  assert_eq "batch_auction: the expired order is the timed-out one" \
    "$(printf '%s\n' "$expired" | nth_num 1)" "$order4"
  invoke "$auction" expire_order --order_id "$order4" >/dev/null
  # Order 5 is still escrowed, so the balance returns to what it was before
  # order 4 less order 5's input.
  assert_eq "batch_auction: expire_order refunds the escrow" \
    "$(balance_of "$ta" "$t1")" "$(( t1_before_order4 - amt5 ))"
  expect_fail "batch_auction: an expired order cannot be expired twice (OrderNotFound)" \
    invoke "$auction" expire_order --order_id "$order4"

  # ── quote, then settle once the window closes ──────────────────────────
  local quote quote_out quote_pool
  quote=$(invoke "$auction" quote_order --order_id "$order1")
  quote_out=$(printf '%s\n' "$quote" | nth_num 1)
  quote_pool=$(printf '%s\n' "$quote" | parse_address)
  assert_gt "batch_auction: quote for order 1 is positive" "$quote_out" 0
  assert_eq "batch_auction: quote routes order 1 to the attested pool" "$quote_pool" "$pool"
  local quote2
  quote2=$(invoke "$auction" quote_order --order_id "$order2" | nth_num 1)
  assert_gt "batch_auction: quote for order 2 is positive" "$quote2" 0

  local t1_pre_settle_a
  t1_pre_settle_a=$(balance_of "$ta" "$t1")
  local opened window_end
  opened=$(invoke "$auction" get_batch_info | nth_num 3)
  window_end=$(( opened + window ))
  e2e_wait_for_ledger_time "$window_end" "batch_auction: batch window"

  local results
  results=$(invoke "$auction" settle_batch)
  assert_eq "batch_auction: settlement reports the two fillable orders" \
    "$(printf '%s\n' "$results" | count_matches '[0-9]+')" "2"
  assert_eq "batch_auction: order 1 output matches the pre-settlement quote" \
    "$(printf '%s\n' "$results" | nth_num 1)" "$quote_out"
  local out2
  out2=$(printf '%s\n' "$results" | nth_num 2)
  assert_gt "batch_auction: order 2 is filled" "$out2" 0
  assert_eq "batch_auction: trader 2 receives order 2's output" \
    "$(balance_of "$ta" "$t2")" "$(( trader_pay + out2 ))"
  assert_eq "batch_auction: the unfillable order is refunded in full" \
    "$(balance_of "$ta" "$t1")" "$(( t1_pre_settle_a + amt5 ))"
  assert_eq "batch_auction: the batch is empty after settlement" \
    "$(invoke "$auction" get_batch_info | nth_num 1)" "0"
  assert_eq "batch_auction: no input tokens remain in escrow (token A)" \
    "$(balance_of "$ta" "$auction")" "0"
  assert_eq "batch_auction: no input tokens remain in escrow (token B)" \
    "$(balance_of "$tb" "$auction")" "0"
  expect_fail "batch_auction: claim_refund with nothing claimable is rejected (NothingToClaim)" \
    invoke_as "$t1_key" "$auction" claim_refund --trader "$t1" --order_id "$order1"

  # ── two-step admin rotation ─────────────────────────────────────────────
  invoke "$auction" propose_admin --current_admin "$admin" --new_admin "$new_admin" >/dev/null
  assert_eq "batch_auction: propose_admin nominates the new admin" \
    "$(invoke "$auction" get_pending_admin | parse_address)" "$new_admin"
  invoke_as "$new_admin_key" "$auction" accept_admin --new_admin "$new_admin" >/dev/null
  assert_eq "batch_auction: accept_admin completes the rotation" \
    "$(invoke "$auction" get_admin | parse_address)" "$new_admin"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  # shellcheck source=scripts/e2e/common.sh
  source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
  e2e_standalone run_batch_auction_flow
fi
