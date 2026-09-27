#!/usr/bin/env bash
# twap_consumer.sh — deploy the TWAP Consumer (twal_consumer has its own module)
# Sourceable module for deploy.sh

deploy_twap_consumer() {
  CURRENT_CONTRACT="twap_consumer"
  log "== twap_consumer =="

  local wasm
  wasm=$(wasm_path twap_consumer)

  if should_skip_persisted "TWAP_CONSUMER_CONTRACT_ID"; then
    TWAP_CONSUMER_CONTRACT_ID=$(get_persisted TWAP_CONSUMER_CONTRACT_ID)
    log "skipping twap_consumer deploy (already at $TWAP_CONSUMER_CONTRACT_ID)"
  else
    if ! should_deploy "twap_consumer"; then
      log "skipping twap_consumer deploy (--only/--skip filter)"
      TWAP_CONSUMER_CONTRACT_ID=$(get_persisted TWAP_CONSUMER_CONTRACT_ID || echo "")
      if [[ -z "$TWAP_CONSUMER_CONTRACT_ID" ]]; then return 0; fi
    else
      CURRENT_STEP="deploy twap_consumer"
      log "deploying twap_consumer: $wasm"
      if [[ ! -f "$wasm" ]]; then
        warn "WASM not found: $wasm — skipping"
        return 0
      fi
      TWAP_CONSUMER_CONTRACT_ID=$(deploy_contract "$wasm")
      persist_var "TWAP_CONSUMER_CONTRACT_ID" "$TWAP_CONSUMER_CONTRACT_ID"
      log "twap_consumer: $TWAP_CONSUMER_CONTRACT_ID"
    fi
  fi

  export TWAP_CONSUMER_CONTRACT_ID
  if [[ -z "${TWAP_CONSUMER_CONTRACT_ID:-}" ]]; then return 0; fi

  if should_skip_persisted "TWAP_CONSUMER_INITIALIZED"; then
    log "skipping twap_consumer initialize (already done)"
  else
    if ! should_deploy "twap_consumer"; then
      log "skipping twap_consumer initialize (--only/--skip filter)"
    else
      CURRENT_STEP="initialize twap_consumer"
      log "initializing twap_consumer keeper=$ADMIN_ADDRESS"
      if ! invoke "$TWAP_CONSUMER_CONTRACT_ID" initialize --keeper "$ADMIN_ADDRESS" >/dev/null 2>&1; then
        if invoke_read "$TWAP_CONSUMER_CONTRACT_ID" -- get_keeper 2>&1 | grep -q "$ADMIN_ADDRESS"; then
          log "twap_consumer already initialized"
        else
          warn "failed to initialize twap_consumer"
        fi
      else
        log "twap_consumer initialized"
      fi
      persist_var "TWAP_CONSUMER_INITIALIZED" "1"
    fi
  fi

  CURRENT_STEP="verify twap_consumer"
  if invoke_read "$TWAP_CONSUMER_CONTRACT_ID" -- get_keeper >/dev/null 2>&1; then
    log "verified twap_consumer keeper"
  else
    warn "twap_consumer verification warning"
  fi
}
