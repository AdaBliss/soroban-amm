#!/usr/bin/env bash
# Explicit contract WASM basenames included in docs/abi.json.
# Keep this sorted; non-contract artifacts such as soroban_amm_sdk and
# flash_loan_receiver are intentionally excluded.
ABI_CONTRACTS=(
  amm
  batch_auction
  batch_router
  cl_position_nft
  concentrated_liquidity
  dex_aggregator
  factory
  governance
  incentive_campaigns
  oracle_aggregator
  pol_vesting
  reserve_manager
  router
  staking
  token
  twal_consumer
  twap_consumer
  v2_to_v3_migration
)
