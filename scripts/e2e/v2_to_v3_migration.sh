#!/usr/bin/env bash
# v2_to_v3_migration.sh — end-to-end flow for moving V2 pool shares into a
# concentrated-liquidity position on a live network: range previews, a
# migration that burns V2 shares and mints a CL position, the rejections the
# contract enforces, a second migration into the same range, and a migration
# after the ledger has moved on.
#
# Every state change is read back through a view, and every rejected call is
# checked with expect_fail. The contract takes its pools per call and checks
# that the factory attests both, so the flow uses factory-created pools.
#
# Self-contained: deploys its own tokens, factory, V2 pool, CL pool and
# migration contract, so it runs without deploy.sh:
#   bash scripts/e2e/v2_to_v3_migration.sh
set -Eeuo pipefail

run_v2_to_v3_migration_flow() {
  CURRENT_FLOW="v2_to_v3_migration"

  local admin="$SOURCE_PUBLIC_KEY"
  local provider="$SOURCE_PUBLIC_KEY"
  local seed="${MIGRATION_POOL_SEED:-10000000}"
  local shares="${MIGRATION_SHARES:-1000000}"
  local outsider_key="${SOURCE_ACCOUNT}-migration-outsider"

  # ── fixtures: tokens, factory, V2 pool, CL pool on the same pair ───────
  local ta tb tc factory v2_pool v3_pool lp
  ta=$(e2e_new_token MTA)
  tb=$(e2e_new_token MTB)
  tc=$(e2e_new_token MTC)
  factory=$(e2e_new_factory)
  v2_pool=$(e2e_new_seeded_pool "$factory" "$ta" "$tb" "$seed")
  invoke "$factory" create_cl_pool --caller "$admin" --token_a "$ta" --token_b "$tb" \
    --fee_bps 30 --initial_tick 0 >/dev/null
  v3_pool=$(invoke "$factory" get_cl_pool --token_a "$ta" --token_b "$tb" --fee_bps 30 | parse_address)
  lp=$(invoke "$factory" get_lp_token --pool "$v2_pool" | parse_address)
  pass "v2_to_v3_migration: fixtures ready (v2=$v2_pool, v3=$v3_pool)"

  # ── initialize ──────────────────────────────────────────────────────────
  local migration
  migration=$(e2e_deploy v2_to_v3_migration)
  invoke "$migration" initialize --admin "$admin" --factory "$factory" >/dev/null
  assert_eq "v2_to_v3_migration: get_admin reads back" \
    "$(invoke "$migration" get_admin | parse_address)" "$admin"
  assert_eq "v2_to_v3_migration: get_factory reads back" \
    "$(invoke "$migration" get_factory | parse_address)" "$factory"
  expect_fail "v2_to_v3_migration: second initialize is rejected" \
    invoke "$migration" initialize --admin "$admin" --factory "$factory"

  # ── preview_range ───────────────────────────────────────────────────────
  # Explicit bounds come back aligned to the CL pool's tick spacing (10 at the
  # 30 bps tier) and kept as given when they are already aligned.
  local explicit lower upper
  explicit=$(invoke "$migration" preview_range --v3_pool "$v3_pool" \
    --tick_lower -600 --tick_upper 600 --range_width_ticks 0)
  lower=$(printf '%s\n' "$explicit" | nth_num 1)
  upper=$(printf '%s\n' "$explicit" | nth_num 2)
  assert_eq "v2_to_v3_migration: explicit lower bound is kept" "$lower" "-600"
  assert_eq "v2_to_v3_migration: explicit upper bound is kept" "$upper" "600"

  # The auto-range sentinel (i32::MIN / i32::MAX) builds a width around the
  # current tick, which is 0 at deploy. The range must be aligned and bracket it.
  local auto auto_lower auto_upper
  auto=$(invoke "$migration" preview_range --v3_pool "$v3_pool" \
    --tick_lower -2147483648 --tick_upper 2147483647 --range_width_ticks 200)
  auto_lower=$(printf '%s\n' "$auto" | nth_num 1)
  auto_upper=$(printf '%s\n' "$auto" | nth_num 2)
  assert_eq "v2_to_v3_migration: auto range lower is tick-spacing aligned" "$(( auto_lower % 10 ))" "0"
  assert_eq "v2_to_v3_migration: auto range upper is tick-spacing aligned" "$(( auto_upper % 10 ))" "0"
  assert_gt "v2_to_v3_migration: auto range brackets the current tick (below)" "$(( 0 - auto_lower ))" "0"
  assert_gt "v2_to_v3_migration: auto range brackets the current tick (above)" "$auto_upper" "0"

  # ── migrate ─────────────────────────────────────────────────────────────
  local lp_before ta_before tb_before migrate_params
  lp_before=$(balance_of "$lp" "$provider")
  ta_before=$(balance_of "$ta" "$provider")
  tb_before=$(balance_of "$tb" "$provider")
  migrate_params=$(cat <<JSON
{"v2_shares":"$shares","min_a":"0","min_b":"0","tick_lower":-600,"tick_upper":600,"range_width_ticks":0,"min_deposit_a":"0","min_deposit_b":"0","deadline":"$(( $(e2e_ledger_time) + 3600 ))"}
JSON
)
  local result
  result=$(invoke "$migration" migrate --provider "$provider" --v2_pool "$v2_pool" \
    --v3_pool "$v3_pool" --params "$migrate_params")
  local deposited_a deposited_b leftover_a leftover_b
  deposited_a=$(printf '%s\n' "$result" | json_num deposited_a)
  deposited_b=$(printf '%s\n' "$result" | json_num deposited_b)
  leftover_a=$(printf '%s\n' "$result" | json_num leftover_a)
  leftover_b=$(printf '%s\n' "$result" | json_num leftover_b)
  assert_gt "v2_to_v3_migration: migration deposits token A into the CL range" "$deposited_a" 0
  assert_gt "v2_to_v3_migration: migration deposits token B into the CL range" "$deposited_b" 0
  assert_eq "v2_to_v3_migration: V2 shares are burned" "$(balance_of "$lp" "$provider")" "$(( lp_before - shares ))"

  local position liquidity
  position=$(invoke "$v3_pool" get_position --provider "$provider" --lower_tick -600 --upper_tick 600)
  liquidity=$(printf '%s\n' "$position" | field_value liquidity)
  assert_gt "v2_to_v3_migration: CL position holds liquidity in the migrated range" "$liquidity" 0

  # The provider's net token change is what the migration left unused, since
  # the withdrawn amounts went into the position.
  assert_eq "v2_to_v3_migration: unused token A returns to the provider" \
    "$(( $(balance_of "$ta" "$provider") - ta_before ))" "$leftover_a"
  assert_eq "v2_to_v3_migration: unused token B returns to the provider" \
    "$(( $(balance_of "$tb" "$provider") - tb_before ))" "$leftover_b"
  assert_eq "v2_to_v3_migration: the migration contract holds no token A" \
    "$(balance_of "$ta" "$migration")" "0"
  assert_eq "v2_to_v3_migration: the migration contract holds no token B" \
    "$(balance_of "$tb" "$migration")" "0"

  # ── rejections ──────────────────────────────────────────────────────────
  local base_params
  base_params=$(cat <<JSON
{"v2_shares":"$shares","min_a":"0","min_b":"0","tick_lower":-600,"tick_upper":600,"range_width_ticks":0,"min_deposit_a":"0","min_deposit_b":"0","deadline":"$(( $(e2e_ledger_time) + 3600 ))"}
JSON
)
  expect_fail "v2_to_v3_migration: zero shares are rejected (ZeroShares)" \
    invoke "$migration" migrate --provider "$provider" --v2_pool "$v2_pool" --v3_pool "$v3_pool" \
      --params "$(printf '%s' "$base_params" | sed 's/"v2_shares":"[0-9]*"/"v2_shares":"0"/')"
  expect_fail "v2_to_v3_migration: an inverted range is rejected (InvalidRange)" \
    invoke "$migration" migrate --provider "$provider" --v2_pool "$v2_pool" --v3_pool "$v3_pool" \
      --params "$(printf '%s' "$base_params" | sed 's/"tick_lower":-600,"tick_upper":600/"tick_lower":600,"tick_upper":-600/')"
  expect_fail "v2_to_v3_migration: a range off the tick spacing is rejected (InvalidRange)" \
    invoke "$migration" migrate --provider "$provider" --v2_pool "$v2_pool" --v3_pool "$v3_pool" \
      --params "$(printf '%s' "$base_params" | sed 's/"tick_lower":-600/"tick_lower":-605/')"
  expect_fail "v2_to_v3_migration: a minimum amount that cannot be met is rejected (SlippageExceeded)" \
    invoke "$migration" migrate --provider "$provider" --v2_pool "$v2_pool" --v3_pool "$v3_pool" \
      --params "$(printf '%s' "$base_params" | sed 's/"min_a":"0"/"min_a":"1000000000000000"/')"

  # A CL pool on a different pair is not the V2 pool's pair (TokenMismatch).
  invoke "$factory" create_cl_pool --caller "$admin" --token_a "$tb" --token_b "$tc" \
    --fee_bps 30 --initial_tick 0 >/dev/null
  local other_v3
  other_v3=$(invoke "$factory" get_cl_pool --token_a "$tb" --token_b "$tc" --fee_bps 30 | parse_address)
  expect_fail "v2_to_v3_migration: a CL pool on another pair is rejected (TokenMismatch)" \
    invoke "$migration" migrate --provider "$provider" --v2_pool "$v2_pool" --v3_pool "$other_v3" \
      --params "$base_params"

  # An account without the V2 shares cannot migrate them.
  local outsider
  outsider=$(e2e_fund_account migration-outsider)
  expect_fail "v2_to_v3_migration: a caller without the V2 shares cannot migrate them" \
    invoke_as "$outsider_key" "$migration" migrate --provider "$outsider" --v2_pool "$v2_pool" \
      --v3_pool "$v3_pool" --params "$base_params"

  # ── second migration into the same range adds to the position ──────────
  local liquidity_before
  liquidity_before=$liquidity
  invoke "$migration" migrate --provider "$provider" --v2_pool "$v2_pool" \
    --v3_pool "$v3_pool" --params "$base_params" >/dev/null
  liquidity=$(invoke "$v3_pool" get_position --provider "$provider" --lower_tick -600 --upper_tick 600 \
    | field_value liquidity)
  assert_gt "v2_to_v3_migration: a second migration adds liquidity to the same range" \
    "$liquidity" "$liquidity_before"

  # ── fresh ledger window: the same migration after the clock moved on ───
  local now
  now=$(e2e_ledger_time)
  e2e_wait_for_ledger_time "$(( now + 10 ))" "v2_to_v3_migration: fresh ledger window"
  liquidity_before=$liquidity
  invoke "$migration" migrate --provider "$provider" --v2_pool "$v2_pool" \
    --v3_pool "$v3_pool" --params "$base_params" >/dev/null
  liquidity=$(invoke "$v3_pool" get_position --provider "$provider" --lower_tick -600 --upper_tick 600 \
    | field_value liquidity)
  assert_gt "v2_to_v3_migration: a migration in a fresh ledger window succeeds" \
    "$liquidity" "$liquidity_before"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  # shellcheck source=scripts/e2e/common.sh
  source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
  e2e_standalone run_v2_to_v3_migration_flow
fi
