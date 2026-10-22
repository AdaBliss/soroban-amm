#!/usr/bin/env bash
# oracle_adapters.sh - deploy AMM and CL TWAP source adapters.

deploy_oracle_adapters() {
  CURRENT_CONTRACT="oracle_adapters"
  if [[ -z "${FACTORY_CONTRACT_ID:-}" || -z "${TWAP_CONSUMER_CONTRACT_ID:-}" ]]; then
    warn "factory or twap_consumer missing - skipping oracle adapters"
    return 0
  fi

  local wasm
  wasm=$(wasm_path oracle_adapters)
  if [[ ! -f "$wasm" ]]; then
    warn "WASM not found: $wasm - skipping"
    return 0
  fi

  if ! should_skip_persisted "AMM_TWAP_ADAPTER_CONTRACT_ID"; then
    AMM_TWAP_ADAPTER_CONTRACT_ID=$(deploy_contract "$wasm")
    invoke "$AMM_TWAP_ADAPTER_CONTRACT_ID" initialize \
      --admin "$ADMIN_ADDRESS" --factory "$FACTORY_CONTRACT_ID" \
      --consumer "$TWAP_CONSUMER_CONTRACT_ID" --mode Amm \
      --window_seconds 1800 --max_age_seconds 3600 --fee_tiers '[]' >/dev/null
    persist_var "AMM_TWAP_ADAPTER_CONTRACT_ID" "$AMM_TWAP_ADAPTER_CONTRACT_ID"
  else
    AMM_TWAP_ADAPTER_CONTRACT_ID=$(get_persisted AMM_TWAP_ADAPTER_CONTRACT_ID)
  fi

  if ! should_skip_persisted "CL_TWAP_ADAPTER_CONTRACT_ID"; then
    CL_TWAP_ADAPTER_CONTRACT_ID=$(deploy_contract "$wasm")
    invoke "$CL_TWAP_ADAPTER_CONTRACT_ID" initialize \
      --admin "$ADMIN_ADDRESS" --factory "$FACTORY_CONTRACT_ID" \
      --consumer "$TWAP_CONSUMER_CONTRACT_ID" --mode ConcentratedLiquidity \
      --window_seconds 1800 --max_age_seconds 3600 --fee_tiers '["1","5","30","100"]' >/dev/null
    persist_var "CL_TWAP_ADAPTER_CONTRACT_ID" "$CL_TWAP_ADAPTER_CONTRACT_ID"
  else
    CL_TWAP_ADAPTER_CONTRACT_ID=$(get_persisted CL_TWAP_ADAPTER_CONTRACT_ID)
  fi

  export AMM_TWAP_ADAPTER_CONTRACT_ID CL_TWAP_ADAPTER_CONTRACT_ID
  invoke_read "$AMM_TWAP_ADAPTER_CONTRACT_ID" get_config >/dev/null
  invoke_read "$CL_TWAP_ADAPTER_CONTRACT_ID" get_config >/dev/null
  log "oracle adapters: AMM=$AMM_TWAP_ADAPTER_CONTRACT_ID CL=$CL_TWAP_ADAPTER_CONTRACT_ID"
}
